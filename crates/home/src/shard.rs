//! The indexes one shard carries: their writers, each frame from split to one buffer
//! append, and their readers.

use std::fmt;
use std::iter;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use buffer::{Buffer, Entry};
use control::Gate;
use control::lease::Lease;
use delivery::Reader;
use types::channel::{self, Slot};
use types::frame::key_set::KeySet;
use types::frame::{self, Draft, Frame, Label, Path};
use types::hash;
use types::time::{Interval, Monotonic, Span, Stamp};

use crate::Refusal;
use crate::index::{Accepted, Index};
use crate::reader;
use crate::split::Split;
use crate::writer::{self, Writer};
use crate::{handoff, named, order, split, state, stored};

/// How long a holder that the open restored keeps control with no connected writer.
const GRACE: Span = Span::from_nanos(10_000_000_000);

/// How often the shard appends the records of named readers whose positions moved.
const INTERVAL: Span = Span::from_nanos(1_000_000_000);

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
/// #     let index = slots.index(stamps);
/// #     slots.data(values, Type::Scalar(Scalar::I64));
/// #     interner.slots().index(stamps);
/// #     interner.slots().data(values, Type::Scalar(Scalar::I64));
/// #     let set = interner.intern(&[Group {
/// #         index: stamps,
/// #         data: &[(values, Type::Scalar(Scalar::I64))],
/// #     }]);
/// #     let config = block::Config::new(1 << 21).expect("the budget fits");
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
/// #         trimmed: false,
/// #     };
/// #     let buffer = Buffer::open(config, &mut slots).await?;
/// #     while clock.now().mesh.is_none() {
/// #         node.clock().sleep(Span::from_nanos(1)).await;
/// #     }
/// use home::{Config, Outcome, Shard, order, reader::Next, writer};
/// use types::authority::Authority;
/// use types::frame::{Draft, Form, Label, Path, Range};
/// use types::time::{Span, Stamp};
///
/// let mut shard = Shard::open(Config {
///     shard: 0,
///     buffer,
///     clock,
///     limits: order::Limits {
///         earliest: Stamp::from_nanos(1),
///         ahead: Span::from_nanos(1_000_000_000),
///     },
/// })
/// .await?;
/// assert!(shard.carry(index, stamps));
/// let charge = home::reader::complete::Charge::Whole;
/// let reader = shard.open_complete(index, 1 << 20, charge).key;
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
/// let Next::Frame(taken) = shard.take(reader.into()) else {
///     panic!("a frame waits");
/// };
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
    decoder: stored::Decoder,
    /// The gate for the next carry of each index that is not carried: the one that
    /// the open restored, or the one that a shed kept.
    kept: hash::Map<Slot, Gate>,
    /// The named records of each index in the buffer, by slot, carried or not: a shed
    /// index whose entries found no block or no room, or that shed with no mesh time,
    /// keeps them, so that a later carry places each record in the part that holds it.
    named: hash::Map<Slot, named::Parts>,
    /// When [`advance`](Shard::advance) next restores records, ends holds, and appends
    /// changed records ([`Shard::arm`]).
    tick: Option<Monotonic>,
}

/// What a shard is built from.
#[derive(Debug)]
pub struct Config {
    /// The shard's number on its node. Each writer key the shard gives carries it,
    /// so it must differ for each shard of the node.
    pub shard: u32,
    /// The shard's buffer, with no entry that waits for a commit: the shard is its
    /// only writer. Index frames, stored headers, and handoff bodies come from its
    /// pool.
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
    /// The stored entry of each accepted group with samples, in group order. Empty
    /// between appends.
    entries: Vec<Entry>,
    outcomes: Vec<Outcome>,
}

/// What became of one group of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A group with samples is queued for the next group commit. A group with no
    /// samples stores nothing and is applied with an empty range, also when a live
    /// write found no room.
    Applied {
        /// The slot of the group's index.
        slot: Slot,
        /// The seq of the group's samples.
        range: frame::Range,
    },
    /// A live group with samples found no room in the ring or the pool. Its seq is a
    /// gap in the log: a complete reader gets the next frame, past it. The gap is
    /// durable only when a later live entry of the index, with samples, a handoff,
    /// or the named readers, is on disk: a restart before that gives the next frame
    /// the same seq. After a [`Shard::shed`] and a [`Shard::carry`] of the index, the
    /// next frame gets the same seq only when no later live entry was appended.
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

/// The future of [`Shard::committed`] and [`Shard::next_commit`]. It does not borrow
/// the shard, and it holds the shard's ring open until it drops. Held past the drop of
/// the shard, it resolves once the buffer ended: with the error that ended the buffer
/// when that came before what it waits for was durable, else with `Ok`.
#[derive(Debug)]
pub struct Commit(buffer::Commit);

impl Future for Commit {
    type Output = Result<(), env::files::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl Shard {
    /// Opens a shard over `config.buffer` that carries no index yet. It restores the
    /// control of each index from its newest handoff record: for a grace from the
    /// open, a recorded holder holds control with no connected writer, and only a
    /// writer of its subject or one with higher authority takes control. An index
    /// with no record, or whose record names no holder, starts empty. It also reads
    /// the newest records of the named readers of each index, which its carry
    /// restores. It needs no mesh time.
    ///
    /// # Errors
    ///
    /// The error of [`Buffer::newest`](buffer::Buffer::newest).
    ///
    /// # Panics
    ///
    /// When a handoff record or a named entry does not decode: another version or a
    /// defect wrote it.
    pub async fn open(config: Config) -> Result<Self, buffer::Error> {
        let Config {
            shard,
            buffer,
            clock,
            limits,
        } = config;
        let holders = handoff::newest(&buffer).await?;
        let (records, named) = named::Parts::newest(&buffer)
            .await?
            .into_iter()
            .map(|(slot, (parts, held))| ((slot, held), (slot, parts)))
            .unzip();
        let now = clock.now().monotonic;
        let grace = Lease::new(GRACE).expect("invariant: the grace is positive");
        let kept = holders
            .into_iter()
            .map(|(slot, last)| (slot, Gate::recover(last, now, grace)))
            .collect();
        Ok(Self {
            number: shard,
            buffer,
            clock,
            limits,
            indexes: Vec::new(),
            places: hash::Map::default(),
            writers: hash::Map::default(),
            next: 0,
            scratch: Scratch::default(),
            readers: reader::Set::new(records),
            decoder: stored::Decoder::default(),
            kept,
            named,
            tick: None,
        })
    }

    /// Carries the index `key` at `slot`, with no connected writer. `key` must be the
    /// index that holds `slot` in the slot table of its writers' key sets. The shard
    /// does not check the pair, and writes `key` in each handoff entry of the index.
    /// Its gate starts from the one that the shard keeps for `slot`: the handoff that
    /// [`open`](Self::open) restored, or the gate that a [`shed`](Self::shed) kept.
    /// Until the grace from the open ends, a restored holder holds control. Each path
    /// continues from its tail in the buffer. Each named reader that the open found
    /// holds its position: a session open at the crash counts as closed at the carry,
    /// or at the first [`advance`](Self::advance) with mesh time when the node has none
    /// yet. Returns whether this call started the carry: `false`, with no change, when
    /// the shard carries `slot` already.
    #[must_use]
    pub fn carry(&mut self, slot: Slot, key: channel::Key) -> bool {
        if self.places.contains_key(&slot) {
            return false;
        }
        let tail = |path| {
            let tail = self.buffer.tail(slot, path);
            order::Tail {
                stamp: tail.stamp,
                seq: tail.seq,
            }
        };
        let live = tail(Path::Live);
        let gate = self.kept.remove(&slot).unwrap_or_default();
        let backfill = tail(Path::Backfill);
        let index = Index::new(self.limits, key, slot, gate, live, backfill);
        let place = self.indexes.len();
        self.places.insert(slot, place);
        self.indexes.push(index);
        let clock::Time { monotonic, mesh } = self.clock.now();
        let mesh = mesh.map(midpoint);
        self.readers.carry(place, slot, live.seq, mesh);
        self.record_named(place, monotonic, mesh);
        true
    }

    /// Stops carrying the index at `slot`. Each named reader of the index stops holding
    /// its position, and the shard appends, in part order, an entry with no named
    /// reader for each part that holds a named record of the index. With no mesh time,
    /// a restart restores the holds. When a part finds no block or no room, a restart
    /// restores the holds of that part and the parts after it. A later
    /// [`carry`](Self::carry) in the same run does not. Its frames stay in the buffer,
    /// so a later carry of `slot` continues each path from its tail in the buffer: the
    /// last entry appended, on disk or not. The shard also keeps the control gate for
    /// that carry when the gate holds a restored holder or a handoff that waits, so a
    /// shed changes no control. A gate that the shard keeps does not change. When its
    /// grace ends before the next carry of `slot`, the first [`advance`](Self::advance)
    /// or [`open_writer`](Self::open_writer) after that carry drops the holder. It does
    /// nothing when the shard does not carry `slot`.
    ///
    /// # Panics
    ///
    /// If a writer or a reader is open on it.
    pub fn shed(&mut self, slot: Slot) {
        let Some(&place) = self.places.get(&slot) else {
            return;
        };
        let mut claims = self.writers.values().flat_map(|session| &session.claims);
        assert!(
            claims.all(|claim| claim.place != place),
            "a writer is open on the index at {slot:?}"
        );
        self.readers.shed(place);
        if let Some(mesh) = self.now() {
            let head = self.head(place, mesh);
            let parts = self.named.entry(slot).or_default();
            parts.append(&self.buffer, &[], head);
            if !parts.held() {
                self.named.remove(&slot);
            }
        }
        let last = self.indexes.len() - 1;
        self.places.remove(&slot);
        let gate = self.indexes.swap_remove(place).gate;
        // With no writer open, a holder is a restored one.
        if gate.holder().is_some() || gate.handoff().is_some() {
            self.kept.insert(slot, gate);
        }
        if place != last {
            self.places.insert(self.indexes[place].slot, place);
            let claims = self.writers.values_mut().flat_map(|s| &mut s.claims);
            for claim in claims.filter(|claim| claim.place == last) {
                claim.place = place;
            }
        }
    }

    /// When the shard next changes with no call, on its monotonic clock: the earliest
    /// end of a holder's control lease or of a restored holder's grace, or the next
    /// tick of the named readers. While a named complete reader is open, or records
    /// changed or wait for mesh time, a tick comes each second; else a tick comes when
    /// the next hold ends. `None` when only a call can change the shard. Only
    /// [`carry`](Self::carry), [`open_writer`](Self::open_writer),
    /// [`close_writer`](Self::close_writer), and
    /// [`open_named_complete`](Self::open_named_complete) can make it earlier by one
    /// second or more. [`open_named_latest`](Self::open_named_latest) and
    /// [`close_reader`](Self::close_reader) can make it earlier by less: a tick before
    /// that, which ends a hold or appends records, comes up to one second late. Takes
    /// O(n) time for n carried indexes.
    #[must_use]
    pub fn deadline(&self) -> Option<Monotonic> {
        let gates = self
            .indexes
            .iter()
            .filter_map(|index| index.gate.deadline());
        gates.chain(self.tick).min()
    }

    /// Applies what came due by now in the gate of each carried index: a holder whose
    /// control lease ran out loses control, and a restored holder that did not reopen
    /// in its grace is dropped. Appends a handoff for each index whose holder changed,
    /// with or without an open writer, as [`close_writer`](Self::close_writer) does.
    /// Before the node first has mesh time, such a handoff waits as one with no room
    /// does. At a tick of the named readers, with mesh time, it restores the records
    /// that wait, ends each hold whose end the midpoint of mesh time passed, and
    /// appends the records of each index whose records changed: an ack moves a named
    /// position with no append. Records that find no room wait for the next tick. Call
    /// it at [`deadline`](Self::deadline); an earlier call changes nothing. After it,
    /// [`deadline`](Self::deadline) is `None` or later than the shard's monotonic time
    /// at the call. Takes O(n) time for n carried indexes. Each holder that loses
    /// control adds O(w) time for the w open writers of its index, and the append of
    /// its handoff. A tick adds O(r) time for the r named readers of each index with
    /// named readers or records.
    pub fn advance(&mut self) {
        let clock::Time { monotonic, mesh } = self.clock.now();
        let mesh = mesh.map(midpoint);
        for place in 0..self.indexes.len() {
            if !self.indexes[place].gate.advance(monotonic) {
                continue;
            }
            let Some(mesh) = mesh else {
                continue;
            };
            append_handoffs(&self.buffer, &mut self.indexes, iter::once(place), mesh);
        }
        if self.tick.is_none_or(|tick| monotonic < tick) {
            return;
        }
        self.tick = None;
        for at in 0..self.readers.ticked().len() {
            let place = self.readers.ticked()[at];
            if let Some(mesh) = mesh {
                self.readers.advance(place, mesh);
            }
            self.record_named(place, monotonic, mesh);
        }
        self.readers.prune();
    }

    /// Mesh time now, as the shard stamps each entry and checks each stamp: the
    /// midpoint of the clock's interval, which never goes back. `None` before the
    /// node first has mesh time.
    #[must_use]
    pub fn now(&self) -> Option<Stamp> {
        self.clocks().map(|(_, mesh)| mesh)
    }

    /// The pool of the shard's buffer. Frames that a writer fills come from it.
    #[must_use]
    pub fn pool(&self) -> &block::Pool {
        self.buffer.pool()
    }

    /// Opens `writer` on each index of its key set, and appends a handoff for each
    /// index where it takes control. A handoff that finds no room waits for the next
    /// write that is not a resend, writer open or close, or end of a lease on its
    /// index. A failed commit fails the next write.
    ///
    /// # Errors
    ///
    /// [`writer::Error::Unsynced`] before the node first has mesh time, else
    /// [`writer::Error::Lease`] for a lease that is not longer than zero. Neither
    /// changes the shard.
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
        let (now, mesh) = self.clocks().ok_or(writer::Error::Unsynced)?;
        let lease = lease.map(writer::lease).transpose()?;
        let control = control::Writer { subject, authority };
        let entries = set.entries();
        let mut claims = Vec::with_capacity(set.groups().len());
        for &entry in set.groups() {
            let place = self.place(entries[entry].slot);
            let key = self.indexes[place].gate.open(control.clone(), lease, now);
            claims.push(Claim { place, key });
        }
        let session = Session { set, claims };
        let places = session.claims.iter().map(|claim| claim.place);
        append_handoffs(&self.buffer, &mut self.indexes, places, mesh);
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
        let places = session.claims.iter().map(|claim| claim.place);
        append_handoffs(&self.buffer, &mut self.indexes, places, mesh);
    }

    /// Applies `frame` to each index it holds, whole or not at all per index. The
    /// frame's bodies go in one append, after the unrecorded handoff of each of its
    /// indexes. Returns the outcome of each present group, in group order.
    ///
    /// # Errors
    ///
    /// [`Error::Resend`] for a frame labeled resend. [`Error::Full`] for a backfill
    /// frame when the ring or the pool has no room, and [`Error::Large`] for a frame
    /// whose bodies no record holds or whose entry passes
    /// [`buffer::Buffer::block_max`]; no seq moves for either. A handoff with no room
    /// decides before the size: the groups with samples of a live frame are lost, and
    /// a backfill frame gets [`Error::Full`]. [`Error::Disk`] after a failed commit,
    /// before any other error but [`Error::Resend`].
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
            let claim = session.claim(group);
            let index = &mut self.indexes[claim.place];
            let checked = index.check(claim.key, path, stamps, now, mesh);
            scratch.checks.push((group, checked, None));
        }
        let places = scratch
            .checks
            .iter()
            .map(|&(group, ..)| session.claim(group).place);
        let recorded = append_handoffs(&self.buffer, &mut self.indexes, places, mesh);
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
        let ready = made.is_ok() && recorded;
        if !ready {
            entries.clear();
        }
        // An empty append still reports a failed commit.
        let appended = room(self.buffer.append(entries.drain(..)));
        let room = match (recorded, appended, made, path) {
            (_, Err(error), ..) => Err(error),
            (true, Ok(_), Err(block::Error::TooLarge { .. }), _) => Err(Error::Large),
            (true, Ok(true), Ok(()), _) => Ok(true),
            (.., Path::Backfill) => Err(Error::Full),
            (.., Path::Live) => Ok(false),
        };
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

    /// Resolves when every group that [`write`](Self::write) gave as
    /// [`Outcome::Applied`] and every handoff appended before the call is on disk: at
    /// once when none of them waits for a commit, else at the end of the group commit
    /// that holds the last of them. A lost group, a group with no samples, and a
    /// handoff that found no room are not appended, so it does not wait for them.
    /// Commits run without this future, so a caller may drop it. Call
    /// [`woken`](Self::woken) after it resolves. [`Commit`] says when one held past the
    /// drop of the shard resolves.
    ///
    /// # Errors
    ///
    /// The future gives the error that ended the buffer when the buffer ended before
    /// those groups and handoffs were on disk.
    pub fn committed(&self) -> Commit {
        Commit(self.buffer.committed())
    }

    /// Resolves at the end of the next group commit after the call, also one in
    /// flight, or gives the file error that ended the buffer. It does not resolve
    /// while nothing is appended. [`Commit`] says when one held past the drop of the
    /// shard resolves.
    #[must_use]
    pub fn next_commit(&self) -> Commit {
        Commit(self.buffer.next_commit())
    }

    /// Opens an unnamed complete reader on the index at `slot`, with a credit of
    /// `limit_bytes`. From the index's live tail on, it gets each live frame with
    /// samples after the commit that holds it, while the bytes it has spent are below
    /// its credit: a frame spends what `charge` says. A frame that finds the credit
    /// spent waits for a [`grant`](Self::grant), and so does each later frame. When a
    /// later commit releases frames of the index while a frame still waits, the reader
    /// reads from that frame on from disk: [`take`](Self::take) gives
    /// [`Next::Disk`](reader::Next::Disk) while the reader has credit, and the reader
    /// gets each frame in order, then live frames again. A live frame that the write
    /// gives as [`Outcome::Lost`] is no miss: the reader gets the next frame, which
    /// starts past it. [`woken`](Self::woken) names the reader once when it goes to
    /// disk with no frame to take, and not while frames wait to be taken.
    ///
    /// Returns its key and its open position. `replaced` is `None`: an unnamed reader
    /// takes over no session.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`.
    #[must_use = "the reader stays open until `close_reader` gets its key"]
    pub fn open_complete(
        &mut self,
        slot: Slot,
        limit_bytes: u64,
        charge: reader::complete::Charge,
    ) -> reader::Opened<reader::complete::Key> {
        let place = self.place(slot);
        let live = self.indexes[place].live_tail();
        let session = self.readers.open_complete(place, live, limit_bytes, charge);
        reader::Opened {
            key: reader::complete::Key { slot, session },
            live,
            replaced: None,
        }
    }

    /// Opens a complete session for the named reader `reader` on the index at `slot`,
    /// as [`open_complete`](Self::open_complete) does, but it starts at the reader's
    /// last acked position while the reader holds one: its session is open, or it
    /// closed less than its `hold` ago. Else it starts at the live tail. It takes over
    /// the open session of the same reader, in either mode. After a close, the reader
    /// holds its position for `hold`, in mesh time. From a position below the live
    /// frames that memory gave readers, the reader reads from disk first, as
    /// [`open_complete`](Self::open_complete) says. The shard appends the record of
    /// each named reader of the index, open or held, in parts of one entry each, which
    /// a restart restores; an ack moves the recorded position at the next tick
    /// ([`advance`](Self::advance)). An open that appends its record takes no frame
    /// until the commit that holds it, which [`woken`](Self::woken) names, and nor
    /// does a takeover with the same hold of a session that waits.
    ///
    /// # Errors
    ///
    /// [`reader::Unopened::Unsynced`] before the node first has mesh time, and
    /// [`reader::Unopened::Full`] when the open adds a record and no part has room for
    /// it: at least 448 records of the longest subjects and names.
    /// [`reader::Unopened::Unrecorded`] when the open adds a record, opens a held one,
    /// or takes over an open session with a new hold, and the buffer has no block or no
    /// room for the entry of the part that holds its record or of a part before it.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`, or `hold` is negative.
    pub fn open_named_complete(
        &mut self,
        slot: Slot,
        reader: reader::named::Key,
        hold: Span,
        limit_bytes: u64,
        charge: reader::complete::Charge,
    ) -> Result<reader::Opened<reader::complete::Key>, reader::Unopened> {
        let place = self.place(slot);
        let (monotonic, now) = self.clocks().ok_or(reader::Unopened::Unsynced)?;
        self.readers.advance(place, now);
        let live = self.indexes[place].live_tail();
        // An open that changes the records appends them before it opens, and its reader
        // takes no frame until they commit: a restart before then would lose the frames
        // that it got and did not ack.
        let records = self.readers.opening(place, &reader, hold, live);
        let mut recorded_at = None;
        if let Some(records) = &records {
            let head = self.head(place, now);
            let parts = self.named.entry(slot).or_default();
            let recorded = parts.open(&self.buffer, records, &reader, head);
            let recorded = recorded.map_err(|named::Full| reader::Unopened::Full {
                limit: named::LIMIT,
            })?;
            recorded_at = Some(recorded.ok_or(reader::Unopened::Unrecorded)?);
        }
        let named = Reader::Named { reader, hold };
        let opened = self.readers.open_named_complete(
            place,
            named,
            live,
            limit_bytes,
            charge,
            recorded_at,
        );
        self.record_named(place, monotonic, Some(now));
        Ok(reader::Opened {
            key: reader::complete::Key {
                slot,
                session: opened.key,
            },
            live: opened.position.live,
            replaced: opened.replaced.map(|session| reader::Key { slot, session }),
        })
    }

    /// Opens an unnamed latest reader on the index at `slot`. It gets the index's
    /// newest live frame, before its commit. Take from it at once:
    /// [`woken`](Self::woken) does not name it for a frame it can take at open.
    ///
    /// Returns its key and its open position. `replaced` is `None`: an unnamed reader
    /// takes over no session.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`.
    #[must_use = "the reader stays open until `close_reader` gets its key"]
    pub fn open_latest(&mut self, slot: Slot) -> reader::Opened<reader::Key> {
        let place = self.place(slot);
        let session = self.readers.open_latest(place).into();
        reader::Opened {
            key: reader::Key { slot, session },
            live: self.latest_live(place),
            replaced: None,
        }
    }

    /// Opens a latest session for the named reader `reader` on the index at `slot`, as
    /// [`open_latest`](Self::open_latest) does. It takes over the open session of the
    /// same reader, in either mode. A complete session that it takes over closes at the
    /// shard's mesh time.
    ///
    /// # Errors
    ///
    /// [`reader::Unsynced`] before the node first has mesh time.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`.
    pub fn open_named_latest(
        &mut self,
        slot: Slot,
        reader: reader::named::Key,
    ) -> Result<reader::Opened<reader::Key>, reader::Unsynced> {
        let place = self.place(slot);
        let (monotonic, now) = self.clocks().ok_or(reader::Unsynced)?;
        let opened = self.readers.open_named_latest(place, reader, now);
        self.record_named(place, monotonic, Some(now));
        Ok(reader::Opened {
            key: reader::Key {
                slot,
                session: opened.key.into(),
            },
            live: self.latest_live(place),
            replaced: opened.replaced.map(|session| reader::Key { slot, session }),
        })
    }

    /// Records that the complete reader `key` has each sample of its index below
    /// `position`. A named reader that opens again starts there. An ack to a closed
    /// reader changes nothing. An ack can make the next [`take`](Self::take) of the
    /// reader give [`Next::Disk`](reader::Next::Disk), and [`woken`](Self::woken) does
    /// not name the reader for it: take after the ack.
    ///
    /// # Errors
    ///
    /// [`reader::Error::Ack`] when the reader is open and `position` drops a path or
    /// adds one.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`, or does not carry the index of `key`.
    pub fn ack(
        &mut self,
        key: reader::complete::Key,
        position: reader::Position,
    ) -> Result<(), reader::Error> {
        let place = self.place(key.slot);
        self.readers.ack(place, key.session, position)
    }

    /// Raises the credit of the complete reader `key` to `limit_bytes` since it
    /// opened. A frame that waits for a grant can be taken while the bytes the reader
    /// spent are below its credit, and [`woken`](Self::woken) does not name the reader
    /// for it: take after the grant. A limit that is not higher changes nothing, and so
    /// does a grant to a closed reader: a grant can arrive after its reader closes.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`, or does not carry the index of `key`.
    pub fn grant(&mut self, key: reader::complete::Key, limit_bytes: u64) {
        let place = self.place(key.slot);
        self.readers.grant(place, key.session, limit_bytes);
    }

    /// Takes the next frame of the reader `key`. [`Next::Empty`](reader::Next::Empty)
    /// when none waits or the reader is closed. A complete reader whose next frames
    /// are on disk gets [`Next::Disk`](reader::Next::Disk).
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`, or does not carry the index of `key`.
    #[must_use]
    pub fn take(&mut self, key: reader::Key) -> reader::Next {
        self.readers
            .take(self.place(key.slot), key.session, &self.buffer)
    }

    /// Gives the frames of `entries` to its reader: a frame of key set `set` of each
    /// data entry with samples. `set` is of one group, of the reader's index; each
    /// series whose channel and type `set` does not hold drops. It drops `entries` when
    /// the reader closed after the read started, also by a takeover or a shed of its
    /// index. Each `set` given to one shard comes from one interner: the key of a key
    /// set names it only in its interner.
    ///
    /// # Errors
    ///
    /// [`block::Error`] when the pool has no block for the first frame of `entries`,
    /// which then gives nothing. A pool error after a frame stops the fill there, and
    /// the reader reads again.
    ///
    /// # Panics
    ///
    /// If `set` does not hold the reader's index.
    pub fn fill(
        &mut self,
        entries: reader::disk::Entries,
        set: &Arc<KeySet>,
    ) -> Result<(), block::Error> {
        let slot = entries.key.slot;
        let Some(&place) = self.places.get(&slot) else {
            return Ok(());
        };
        let durable = self.buffer.durable(slot, Path::Live).seq;
        let (pool, decoder) = (self.buffer.pool(), &mut self.decoder);
        self.readers.fill(place, entries, durable, set, |stored| {
            decoder.frame(pool, set, stored)
        })
    }

    /// Closes the reader `key`. Its waiting frames do not go out, and
    /// [`woken`](Self::woken) does not name it. A named complete reader holds its
    /// position for its `hold` after the close, and the shard appends the records of
    /// its index, as [`open_named_complete`](Self::open_named_complete) does. A close
    /// of a closed reader changes nothing.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`, or does not carry the index of `key`.
    pub fn close_reader(&mut self, key: reader::Key) {
        let place = self.place(key.slot);
        let clock::Time { monotonic, mesh } = self.clock.now();
        let mesh = mesh.map(midpoint);
        self.readers.close(place, key.session, mesh);
        self.record_named(place, monotonic, mesh);
    }

    /// Replaces `keys` with the readers to wake since the last call, each once, in slot
    /// order and with the latest readers of an index first. Call it after each write,
    /// because a latest reader gets a frame before its commit, and after each commit.
    /// Complete readers first get the live frames now on disk. A key is a hint: take
    /// from each until [`take`](Self::take) gives [`Next::Empty`](reader::Next::Empty),
    /// and read each
    /// [`Next::Disk`](reader::Next::Disk) that it gives. When a commit ended since the
    /// last call, it reads each index with live frames queued for complete readers;
    /// else it reads none. Called so, with the same `keys` each time, a call allocates
    /// only when it gives more keys than each call before, or when more frames wait for
    /// one complete reader than have waited for that reader before.
    pub fn woken(&mut self, keys: &mut Vec<reader::Key>) {
        self.readers.woken(&self.buffer, keys);
    }

    /// A reading of the monotonic clock, and mesh time at it: the midpoint of the
    /// clock's interval, which never goes back. The latest edge can go back as the
    /// error shrinks, and is a century out while the error is unknown. `None` before
    /// the node first has mesh time.
    fn clocks(&self) -> Option<(Monotonic, Stamp)> {
        let clock::Time { monotonic, mesh } = self.clock.now();
        Some((monotonic, midpoint(mesh?)))
    }

    /// Both clocks now, as [`clocks`](Self::clocks) gives them.
    ///
    /// # Panics
    ///
    /// Before the node first has mesh time. No writer or named reader opens before it,
    /// and mesh time stays once known.
    fn time(&self) -> (Monotonic, Stamp) {
        let now = self.clocks();
        now.expect("invariant: mesh time stays once known")
    }

    /// Appends the records of the named readers of the index at `place` when they
    /// changed and the node has mesh time `mesh`, and marks them recorded when each
    /// part appended, then arms the tick for the index at monotonic time `monotonic`.
    /// Call it after each change of the named readers of the index.
    fn record_named(
        &mut self,
        place: usize,
        monotonic: Monotonic,
        mesh: Option<Stamp>,
    ) {
        if let Some(mesh) = mesh
            && self.readers.changed(place)
        {
            let records = self.readers.records(place);
            let head = self.head(place, mesh);
            let parts = self.named.entry(head.slot).or_default();
            if parts.append(&self.buffer, &records, head) {
                self.readers.recorded(place, !records.is_empty());
            }
        }
        self.arm(place, monotonic, mesh);
    }

    /// The head of a state entry of the index at `place` at mesh time `mesh`.
    fn head(&self, place: usize, mesh: Stamp) -> state::Head {
        let index = &self.indexes[place];
        state::Head {
            index: index.key,
            slot: index.slot,
            first: index.live_tail(),
            stored_at: mesh,
        }
    }

    /// Makes the tick no later than the next one that the named readers of the index
    /// at `place` need, at monotonic time `monotonic` and mesh time `mesh`: one
    /// interval later while a named complete reader of it is open, or its records
    /// changed or wait for mesh time, else when its next hold ends. A later tick for
    /// another index stays: that tick arms each index again.
    ///
    /// # Panics
    ///
    /// If a named reader holds and `mesh` is `None`: a hold starts only with mesh
    /// time, which stays once known.
    fn arm(&mut self, place: usize, monotonic: Monotonic, mesh: Option<Stamp>) {
        let next = if self.readers.busy(place) {
            Some(monotonic + INTERVAL)
        } else {
            self.readers.deadline(place).map(|end| {
                let mesh = mesh.expect("invariant: a hold starts with mesh time");
                monotonic + (end - mesh).max(Span::ZERO)
            })
        };
        self.tick = self.tick.into_iter().chain(next).min();
    }

    /// The open position of a latest session on the index at `place`: the first seq
    /// of its newest live frame, or its live tail when it has none.
    fn latest_live(&self, place: usize) -> u64 {
        self.readers
            .newest(place)
            .unwrap_or_else(|| self.indexes[place].live_tail())
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
}

impl Session {
    /// The writer's claim on the index of `group`.
    fn claim(&self, group: u32) -> Claim {
        let group = usize::try_from(group).expect("invariant: a usize holds a u32");
        self.claims[group]
    }
}

/// The midpoint of `mesh`, the best guess of the true time.
fn midpoint(mesh: Interval) -> Stamp {
    Stamp::from_nanos(mesh.earliest.nanos().midpoint(mesh.latest.nanos()))
}

/// Freezes the index frame from `split` of each accepted group of `checks` with
/// samples into its check, and pushes its stored entry onto `entries`, in group order,
/// at mesh time `stored_at`. A group with no samples gets no frame and no entry.
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
        if let Ok(accepted) = checked
            && let Some(last) = accepted.last()
        {
            let draft = split.frame(pool, *group)?;
            let frame = frozen.insert(accepted.freeze(draft, *group));
            entries.push(stored::entry(pool, frame, set, last, stored_at)?);
        }
    }
    Ok(())
}

/// Appends the unrecorded handoff of the index at each of `places`, each alone, on the
/// live path at mesh time `mesh`, and marks it recorded when [`handoff::append`] says
/// that it needs no later append. Returns whether every handoff was recorded.
fn append_handoffs(
    buffer: &Buffer,
    indexes: &mut [Index],
    places: impl Iterator<Item = usize>,
    mesh: Stamp,
) -> bool {
    let mut all = true;
    for place in places {
        let index = &mut indexes[place];
        let Some((handoff, first)) = index.handoff() else {
            continue;
        };
        let head = state::Head {
            index: index.key,
            slot: index.slot,
            first,
            stored_at: mesh,
        };
        if handoff::append(buffer, handoff, head) {
            index.gate.recorded();
        } else {
            all = false;
        }
    }
    all
}

/// Spends the seq of each accepted group of `checks`, and makes the outcome of each
/// present group into `out`: applied when the append found `room` or the group has no
/// samples, else lost. Each frame goes to `readers`.
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
        let claim = session.claim(group);
        let index = &mut indexes[claim.place];
        let slot = index.slot;
        out.push(match checked {
            // A group with no samples stores nothing, so it needs no room.
            Ok(accepted) if accepted.last().is_none() => {
                let range = range(&accepted.seq());
                index.spend(accepted);
                Outcome::Applied { slot, range }
            }
            Ok(accepted) if room => {
                let seq = accepted.seq();
                let range = range(&seq);
                index.spend(accepted);
                let frame = frozen.expect("invariant: a stored frame was frozen");
                readers.applied(claim.place, frame, &session.set, seq);
                Outcome::Applied { slot, range }
            }
            Ok(accepted) => {
                let range = range(&accepted.seq());
                index.spend(accepted);
                readers.lost(claim.place, frozen);
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
    use std::path::{Path as FilePath, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::Waker;
    use std::time::Instant;

    use block::{Pool, Unique};
    use buffer::{Layout, Mark};
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
    use crate::common::{
        self, create_interner, create_pool, data_type, intern, key, values,
    };
    use crate::reader::complete::Charge;

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
        /// Whether each ring that [`Test::create_buffer`] makes trims its tail when an
        /// append does not fit.
        trimmed: bool,
    }

    impl Test {
        /// What a test on a shard of `node` gets, with the node's clock running.
        fn new(node: sim::node::Node, tasks: Tasks) -> Self {
            let pool = create_pool(POOL);
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
                trimmed: false,
            }
        }

        /// A shard over the ring of the node, made with `area` bytes when it is new,
        /// with slots 0 to `slots` assigned and no index carried, once the node has
        /// mesh time.
        async fn open(&self, area: u64, slots: u32) -> Shard {
            let buffer = self.create_buffer(area, BODY_MAX, slots).await;
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
            Self::with(shard, buffer, self.reader.clone()).await
        }

        /// A shard over the ring of the node that never has mesh time, with no index
        /// carried.
        async fn unsynced(&self) -> Shard {
            let buffer = self.create_buffer(AREA, BODY_MAX, 4).await;
            // A clock that never runs never has mesh time.
            let (_, reader) = clock::Clock::new(self.clock.clone());
            Self::with(0, buffer, reader).await
        }

        /// A shard of the number `shard` over `buffer`, with the clocks of `reader`.
        async fn with(shard: u32, buffer: Buffer, reader: clock::Reader) -> Shard {
            let config = Config {
                shard,
                buffer,
                clock: reader,
                limits: LIMITS,
            };
            Shard::open(config).await.expect("opens")
        }

        /// Mesh time now: the midpoint of the clock's interval.
        fn now(&self) -> Stamp {
            let now = self.reader.now().mesh.expect("the node has mesh time");
            Stamp::from_nanos(now.earliest.nanos().midpoint(now.latest.nanos()))
        }

        /// The ring of the node, made with `area` bytes and bodies of `body_max` when
        /// it is new, with slots 0 to `slots` assigned.
        async fn create_buffer(
            &self,
            area: u64,
            body_max: usize,
            slots: u32,
        ) -> Buffer {
            let config = buffer::Config {
                files: self.node.files(),
                dir: PathBuf::from(DIR),
                pool: Rc::clone(&self.pool),
                clock: self.clock.clone(),
                tasks: self.tasks.clone(),
                entropy: self.entropy.clone(),
                layout: Layout::new(area, body_max).expect("a ring"),
                commit: COMMIT,
                trimmed: self.trimmed,
            };
            let mut assigned = Slots::new();
            for n in 0..slots {
                assigned.index(key(Slot::new(n)));
            }
            Buffer::open(config, &mut assigned).await.expect("opens")
        }

        /// A shard as [`open`](Self::open) makes, that carries the indexes of
        /// [`two_indexes`].
        async fn shard(&self, area: u64) -> Shard {
            let mut shard = self.open(area, 4).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
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
                    assert!(shard.carry(Slot::new(n), key(Slot::new(n))));
                    Group {
                        index: key(Slot::new(n)),
                        data: &[],
                    }
                })
                .collect();
            (shard, intern(&groups))
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
        let (sim, node) = create_node(seed);
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

    fn create_node(seed: u64) -> (sim::Sim, sim::node::Node) {
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
        intern(&[
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
        intern(&[Group {
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

    /// A frame that no block of the shard holds: entry 0 with 240000 stamps from
    /// `first`, entry 1 with as many scattered values, and the series of `more`. The
    /// shard's largest block is 1835008 bytes. The writer's pool has larger blocks,
    /// and scattered values do not compress.
    fn over_block(set: &KeySet, first: i64, more: &[(usize, &[i64])]) -> Draft {
        let len = 240_000;
        let stamps: Vec<i64> = (first..).take(len).collect();
        let values = scattered(len);
        let mut series: Vec<(usize, &[i64])> = vec![(0, &stamps), (1, &values)];
        series.extend_from_slice(more);
        frame(&create_pool(4 * POOL), set, &series)
    }

    /// A frame that no record of `BODY_MAX` holds: entry 0 with 600 stamps from `first`
    /// and entry 1 with as many scattered values, which do not compress.
    fn over_record(pool: &Pool, set: &KeySet, first: i64) -> Draft {
        let len = 600;
        let stamps: Vec<i64> = (first..).take(len).collect();
        frame(pool, set, &[(0, &stamps), (1, &scattered(len))])
    }

    /// The error of a failed sync of the ring, which the commit of `shard` gives.
    async fn failed_sync(shard: &Shard) -> env::files::Error {
        let failed = env::files::Error::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        };
        assert_eq!(shard.committed().await, Err(failed.clone()));
        failed
    }

    /// One poll of `commit`.
    fn polled(commit: &mut Commit) -> Poll<Result<(), env::files::Error>> {
        Pin::new(commit).poll(&mut Context::from_waker(Waker::noop()))
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
        check_panics(seed, "the shard does not carry the index at Slot(3)", call);
    }

    /// Asserts that `call` panics with `message` on a shard that carries slots 0 and
    /// 2.
    fn check_panics(seed: u64, message: &str, call: fn(&mut Shard)) {
        let (mut sim, _handle) = start(seed, move |test| async move {
            call(&mut test.shard(AREA).await);
        });
        assert_eq!(
            sim.run(),
            Err(sim::Error::Panicked {
                thread: DIR.into(),
                message: message.into(),
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
        shard.open_complete(slot, CREDIT, Charge::Whole).key.into()
    }

    /// Each frame that `reader` takes now, then the [`reader::Next`] after them.
    fn drain(
        shard: &mut Shard,
        reader: impl Into<reader::Key>,
    ) -> (Vec<Frame>, reader::Next) {
        let reader = reader.into();
        let mut frames = Vec::new();
        loop {
            match shard.take(reader) {
                reader::Next::Frame(frame) => frames.push(frame),
                end @ (reader::Next::Empty | reader::Next::Disk(_)) => {
                    return (frames, end);
                }
            }
        }
    }

    /// The seq of the index group `group` of each frame that `reader` takes before
    /// [`reader::Next::Empty`].
    ///
    /// # Panics
    ///
    /// If `reader` reads from disk.
    #[track_caller]
    fn taken(shard: &mut Shard, reader: reader::Key, group: u32) -> Vec<Range> {
        let (frames, end) = drain(shard, reader);
        let ranges = ranges(&frames, group);
        assert!(
            matches!(end, reader::Next::Empty),
            "{end:?} after {ranges:?}"
        );
        ranges
    }

    /// Reads past which no test reads.
    const READS: usize = 1000;

    /// The key set that the hub gives a read from disk of the index at `slot`, 0 or 2:
    /// one group, with a data channel of type `I64` at each other slot below 64 that is
    /// no index. One interner interns the sets of both indexes in turn at each call, so
    /// a test cannot give one shard two sets with the same key.
    ///
    /// # Panics
    ///
    /// If `slot` is not 0 or 2.
    fn disk_set(slot: Slot) -> Arc<KeySet> {
        const INDEXES: [u32; 2] = [0, 2];
        let data: Vec<_> = (0..64)
            .filter(|n| !INDEXES.contains(n))
            .map(|n| (key(Slot::new(n)), Type::Scalar(Scalar::I64)))
            .collect();
        let mut interner = create_interner(&INDEXES, &[]);
        let [zero, two] = INDEXES.map(|n| {
            interner.intern(&[Group {
                index: key(Slot::new(n)),
                data: &data,
            }])
        });
        match slot.get() {
            0 => zero,
            2 => two,
            n => panic!("a test reads index {n} from disk"),
        }
    }

    /// Each frame that `reader` takes, with each read from disk that
    /// [`reader::Next::Disk`] asks for, then the [`reader::Next`] after them and the
    /// count of reads.
    ///
    /// # Panics
    ///
    /// After `READS` reads, or when a read fails.
    async fn read(
        shard: &mut Shard,
        reader: impl Into<reader::Key>,
    ) -> (Vec<Frame>, reader::Next, usize) {
        let reader = reader.into();
        let set = disk_set(reader.slot);
        let mut frames = Vec::new();
        for reads in 0..READS {
            let (taken, end) = drain(shard, reader);
            frames.extend(taken);
            let reader::Next::Disk(disk) = end else {
                return (frames, end, reads);
            };
            let read = disk.read().await.expect("the read ends");
            shard.fill(read, &set).expect("the pool has a block");
        }
        panic!("{READS} reads");
    }

    /// The seq of the index group `group` of each frame that `reader` takes, with
    /// each read from disk, before [`reader::Next::Empty`].
    ///
    /// # Panics
    ///
    /// As [`read`], or if `reader` gets another [`reader::Next`].
    async fn caught_up(
        shard: &mut Shard,
        reader: impl Into<reader::Key>,
        group: u32,
    ) -> Vec<Range> {
        let (frames, end, _) = read(shard, reader).await;
        let ranges = ranges(&frames, group);
        assert!(
            matches!(end, reader::Next::Empty),
            "{end:?} after {ranges:?}"
        );
        ranges
    }

    fn ranges(frames: &[Frame], group: u32) -> Vec<Range> {
        frames
            .iter()
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
    fn changes_nothing_at_a_second_carry_of_an_index() {
        run(99, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            let latest = shard.open_latest(Slot::new(0));
            assert_eq!((latest.live, latest.replaced), (0, None));
            let latest = latest.key;
            assert!(!shard.carry(Slot::new(0), key(Slot::new(0))));
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(0, 1, 1)][..]));
            assert_eq!(taken(&mut shard, latest, 0), [Range { seq: 1, count: 1 }]);
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
            assert!(shard.carry(Slot::new(1), key(Slot::new(1))));
            let set = intern(&[Group {
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
    fn gives_a_complete_reader_each_frame_before_a_live_frame_lost_for_no_room() {
        run(6, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(1 << 16).await;
            let reader = complete(&mut shard, Slot::new(0));
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let mut applied = Vec::new();
            for n in 0..u32::MAX {
                let at = u64::from(n);
                let stamp = 10 + i64::from(n);
                let write = frame(&test.pool, &set, &[(0, &[stamp]), (1, &[1])]);
                let written = shard.write(a, LIVE, write).expect("written");
                if written == [lost(0, at, 1)] {
                    break;
                }
                assert_eq!(written, [self::applied(0, at, 1)]);
                applied.push(seq(at, 1));
                shard.committed().await.expect("the commit ends");
            }
            woken(&mut shard);
            assert_eq!(taken(&mut shard, reader, 0), applied);
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
    fn opens_a_latest_session_at_the_first_seq_of_its_newest_frame() {
        run(17, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            let next = frame(&test.pool, &set, &[(0, &[20, 30]), (1, &[2, 3])]);
            assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(0, 1, 2)][..]));
            let latest = shard.open_latest(Slot::new(0));
            assert_eq!(latest.live, 1);
            assert_eq!(taken(&mut shard, latest.key, 0), [seq(1, 2)]);
        });
    }

    #[test]
    fn opens_a_latest_session_at_the_first_seq_of_a_lost_newest_frame() {
        run(16, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            let live = frame(&test.pool, &set, &[(0, &[20, 30]), (1, &[2, 3])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, live), Ok(&[lost(0, 1, 2)][..]));
            let latest = shard.open_latest(Slot::new(0));
            assert_eq!(latest.live, 1);
            assert_eq!(taken(&mut shard, latest.key, 0), [seq(1, 2)]);
            drop(blocks);
        });
    }

    #[test]
    fn opens_a_latest_session_at_the_frame_it_gets_after_a_loss_with_no_block() {
        run(16, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first: [(usize, &[i64]); 3] = [(0, &[5]), (1, &[0]), (2, &[1])];
            let first = frame(&test.pool, &set, &first);
            assert_eq!(
                shard.write(a, LIVE, first),
                Ok(&[applied(0, 0, 1), applied(2, 0, 1)][..])
            );
            let stamps: Vec<_> = (2..=101).collect();
            let series: [(usize, &[i64]); 3] = [(0, &[10]), (1, &[1]), (2, &stamps)];
            let live = frame(&test.pool, &set, &series);
            let twin = frame(&test.pool, &set, &series);
            let room = [80, 56].map(|len| test.pool.alloc(len).expect("a block"));
            let blocks = test.fill();
            drop(room);
            assert_eq!(
                shard.write(a, LIVE, live),
                Ok(&[lost(0, 1, 1), lost(2, 1, 100)][..])
            );
            let latest = shard.open_latest(Slot::new(2));
            assert_eq!(latest.live, 0);
            assert_eq!(taken(&mut shard, latest.key, 1), [seq(0, 1)]);
            drop((twin, blocks));
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
    fn gives_each_complete_reader_the_frame_after_one_the_pool_could_not_freeze() {
        run(16, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let zero = complete(&mut shard, Slot::new(0));
            let two = complete(&mut shard, Slot::new(2));
            let stamps: Vec<_> = (1..=100).collect();
            let series: [(usize, &[i64]); 3] = [(0, &[10]), (1, &[1]), (2, &stamps)];
            let live = frame(&test.pool, &set, &series);
            let twin = frame(&test.pool, &set, &series);
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let room = [80, 56].map(|len| test.pool.alloc(len).expect("a block"));
            let blocks = test.fill();
            drop(room);
            assert_eq!(
                shard.write(a, LIVE, live),
                Ok(&[lost(0, 0, 1), lost(2, 0, 100)][..])
            );
            drop((twin, blocks));
            let next: [(usize, &[i64]); 3] = [(0, &[20]), (1, &[2]), (2, &[200])];
            let next = frame(&test.pool, &set, &next);
            assert_eq!(
                shard.write(a, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 100, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            woken(&mut shard);
            assert_eq!(taken(&mut shard, zero, 0), [seq(1, 1)]);
            assert_eq!(taken(&mut shard, two, 1), [seq(100, 1)]);
        });
    }

    #[test]
    fn refuses_a_frame_over_the_record_and_spends_nothing() {
        run(38, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            for (label, stamp) in [(LIVE, 700), (BACKFILL, 5)] {
                let over_record = over_record(&test.pool, &set, 10);
                assert_eq!(shard.write(a, label, over_record), Err(Error::Large));
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
    fn refuses_a_frame_whose_group_is_over_the_block() {
        run(45, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            for (label, stamp) in [(LIVE, 300_000), (BACKFILL, 5)] {
                let over_block = over_block(&set, 10, &[(2, &[stamp])]);
                assert_eq!(shard.write(a, label, over_block), Err(Error::Large));
                let small = frame(&test.pool, &set, &[(0, &[stamp]), (1, &[1])]);
                assert_eq!(shard.write(a, label, small), Ok(&[applied(0, 0, 1)][..]));
            }
        });
    }

    #[test]
    fn records_a_waiting_handoff_before_a_frame_over_the_record() {
        run(39, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let over_record = over_record(&test.pool, &set, 10);
            let blocks = test.fill();
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            drop(blocks);
            assert_eq!(shard.write(a, LIVE, over_record), Err(Error::Large));
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
            let written = shard.write(a, LIVE, over_record(&test.pool, &set, 10));
            assert_eq!(written, Err(Error::Large));
            let b = shard.open_writer(writer("b", 2, &set)).expect("synced");
            assert!(shard.indexes[0].handoff().is_some(), "no room at the open");
            // The handoff is appended before the bodies, so the size is never checked.
            let written = shard.write(b, LIVE, over_record(&test.pool, &set, 10));
            assert_eq!(written, Ok(&[lost(0, 0, 600)][..]));
            let over_block = over_block(&set, 1000, &[(2, &[stamp])]);
            assert_eq!(
                shard.write(b, LIVE, over_block),
                Ok(&[lost(0, 600, 240_000), lost(2, 13, 1)][..])
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
            let zero = intern(&[Group {
                index: key(Slot::new(0)),
                data: &[],
            }]);
            let two = intern(&[Group {
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
    fn does_not_renew_the_lease_for_a_frame_over_the_record() {
        run(95, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            let over_record = over_record(&test.pool, &set, 10);
            assert_eq!(shard.write(a, LIVE, over_record), Err(Error::Large));
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
            let mesh = shard.now().expect("the node has mesh time");
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
            let failed = failed_sync(&shard).await;
            let disk = Error::Disk(failed);
            assert_eq!(
                disk.to_string(),
                "a commit failed: sync of shard-0/ring failed with OS error 5"
            );
            let resend = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            assert_eq!(shard.write(a, Label::Resend, resend), Err(Error::Resend));
            let write = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            assert_eq!(shard.write(a, LIVE, write), Err(disk.clone()));
            let refused = frame(&test.pool, &set, &[(2, &[20])]);
            assert_eq!(shard.write(b, LIVE, refused), Err(disk.clone()));
            let empty = frame(&test.pool, &set, &[(0, &[]), (1, &[])]);
            assert_eq!(shard.write(a, LIVE, empty), Err(disk.clone()));
            let live = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
            let backfill = frame(&test.pool, &set, &[(0, &[1]), (1, &[1])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, live), Err(disk.clone()));
            assert_eq!(shard.write(a, BACKFILL, backfill), Err(disk));
            drop(blocks);
        });
    }

    #[test]
    fn fails_a_large_frame_after_a_failed_sync() {
        /// Writes, on each path, a frame over the block and a frame over the record.
        /// Each passes the order check of its path, so its size counts.
        fn write(test: &Test, shard: &mut Shard, a: writer::Key, expected: &Error) {
            let set = two_indexes();
            for (label, first) in [(LIVE, 600_000), (BACKFILL, 100)] {
                let over_block = over_block(&set, first, &[]);
                let written = shard.write(a, label, over_block);
                assert_eq!(written, Err(expected.clone()), "{label:?}");
                let over_record = over_record(&test.pool, &set, first);
                let written = shard.write(a, label, over_record);
                assert_eq!(written, Err(expected.clone()), "{label:?}");
            }
        }

        run(112, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 2, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[500_000]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            shard.committed().await.expect("the commit ends");
            write(&test, &mut shard, a, &Error::Large);
            test.node.fail_file(FilePath::new(RING), Operation::Sync);
            let second = frame(&test.pool, &set, &[(0, &[500_001]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, second), Ok(&[applied(0, 1, 1)][..]));
            let failed = failed_sync(&shard).await;
            write(&test, &mut shard, a, &Error::Disk(failed));
        });
    }

    #[test]
    fn fails_a_frame_whose_handoff_waits_after_a_failed_sync() {
        run(114, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            test.node.fail_file(FilePath::new(RING), Operation::Sync);
            let write = frame(&test.pool, &set, &[(0, &[500_000]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 0, 1)][..]));
            let disk = Error::Disk(failed_sync(&shard).await);
            let b = shard.open_writer(writer("b", 3, &set)).expect("synced");
            let writers = create_pool(4 * POOL);
            let small = |stamp: i64| frame(&writers, &set, &[(0, &[stamp]), (1, &[1])]);
            // On each path: a frame of one sample, a frame that the order check
            // refuses, and a frame over the block.
            let mut write = |state: &str| {
                for (label, first, refused) in
                    [(LIVE, 600_000, 100), (BACKFILL, 100, 500_000)]
                {
                    let frames =
                        [small(first), small(refused), over_block(&set, first, &[])];
                    for (at, draft) in frames.into_iter().enumerate() {
                        let written = shard.write(b, label, draft);
                        assert_eq!(
                            written.as_ref(),
                            Err(&disk),
                            "{state}, {label:?}, {at}"
                        );
                    }
                }
            };
            write("the append of the handoff fails");
            // The size of the handoff to b: a block for it, and none for the frame.
            let room = test.pool.alloc(2).expect("a block");
            let _blocks = test.fill();
            write("no block holds the handoff");
            drop(room);
            write("no block holds the frame");
        });
    }

    #[test]
    fn refuses_a_backfill_frame_over_the_block_whose_handoff_finds_no_room() {
        run(113, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let _a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let blocks = test.fill();
            let b = shard.open_writer(writer("b", 3, &set)).expect("synced");
            let written = shard.write(b, BACKFILL, over_block(&set, 100, &[]));
            assert_eq!(written, Err(Error::Full));
            drop(blocks);
            let written = shard.write(b, BACKFILL, over_block(&set, 100, &[]));
            assert_eq!(written, Err(Error::Large));
        });
    }

    #[test]
    fn opens_no_writer_before_the_node_has_mesh_time() {
        run(60, |test| async move {
            let mut shard = test.unsynced().await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            assert_eq!(shard.now(), None);
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

    /// A sim whose node lost power after writer `a` took control of both indexes of
    /// [`two_indexes`] and wrote one frame, and after it closed when `closed`.
    fn cut_after(seed: u64, closed: bool) -> (sim::Sim, sim::node::Node) {
        let (mut sim, node) = create_node(seed);
        sim.run_on(&node, move |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            assert_eq!(
                shard.write(a, LIVE, first),
                Ok(&[applied(0, 0, 1), applied(2, 0, 1)][..])
            );
            if closed {
                shard.close_writer(a);
            }
            shard.committed().await.expect("the commit ends");
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        (sim, node)
    }

    #[test]
    fn reserves_control_for_the_holder_from_before_a_power_cut_until_the_grace_ends() {
        let (mut sim, node) = cut_after(170, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let b = shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[refused(0, Refusal::Reserved), refused(2, Refusal::Reserved)][..])
            );
            test.clock.sleep(GRACE).await;
            let next = frame(&test.pool, &set, &[(0, &[30]), (1, &[3]), (2, &[30])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 2);
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn gives_control_to_the_holder_from_before_a_power_cut_when_it_reopens() {
        let (mut sim, node) = cut_after(171, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(a, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn gives_control_to_a_higher_authority_in_the_grace_after_a_power_cut() {
        let (mut sim, node) = cut_after(172, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let c = shard.open_writer(writer("c", 2, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(c, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn starts_an_empty_gate_after_a_power_cut_when_the_handoff_names_no_holder() {
        let (mut sim, node) = cut_after(173, true);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    /// The grace runs from the open, not from the first carry.
    #[test]
    fn gives_control_at_a_carry_after_the_grace_from_the_open_after_a_power_cut() {
        let (mut sim, node) = cut_after(174, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.open(AREA, 4).await;
            test.clock.sleep(GRACE).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn reserves_control_after_a_shed_and_a_carry_in_the_grace() {
        let (mut sim, node) = cut_after(183, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            shard.shed(Slot::new(0));
            shard.shed(Slot::new(2));
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[refused(0, Refusal::Reserved), refused(2, Refusal::Reserved)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    /// Cuts the power of `node`, then checks that the open restores no holder: a
    /// writer of another subject at the authority of `a` takes control.
    fn check_no_holder_after_a_cut(mut sim: sim::Sim, node: &sim::node::Node) {
        sim.crash(node, sim::Crash::Power);
        sim.run_on(node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            assert_eq!(shard.deadline(), None, "no holder is restored");
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
        })
        .expect("the run after the second cut ends");
    }

    #[test]
    fn records_the_drop_of_a_restored_holder_with_no_writer_at_the_end_of_the_grace() {
        let (mut sim, node) = cut_after(184, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let mut shard = test.shard(AREA).await;
            test.clock.sleep(GRACE).await;
            shard.advance();
            assert_eq!(shard.deadline(), None);
            shard.committed().await.expect("the commit ends");
        })
        .expect("the run after the first cut ends");
        check_no_holder_after_a_cut(sim, &node);
    }

    #[test]
    fn records_the_drop_of_a_restored_holder_after_a_shed_and_a_carry_in_the_grace() {
        let (mut sim, node) = cut_after(185, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let mut shard = test.shard(AREA).await;
            shard.shed(Slot::new(0));
            shard.shed(Slot::new(2));
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            test.clock.sleep(GRACE).await;
            shard.advance();
            shard.committed().await.expect("the commit ends");
        })
        .expect("the run after the first cut ends");
        check_no_holder_after_a_cut(sim, &node);
    }

    #[test]
    fn records_the_drop_at_a_carry_after_a_grace_that_ends_while_shed() {
        let (mut sim, node) = cut_after(191, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let mut shard = test.shard(AREA).await;
            shard.shed(Slot::new(0));
            shard.shed(Slot::new(2));
            test.clock.sleep(GRACE).await;
            assert_eq!(shard.deadline(), None, "a kept gate has no deadline");
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            assert!(shard.deadline().is_some_and(|due| due <= test.clock.now()));
            shard.advance();
            assert_eq!(shard.deadline(), None);
            shard.committed().await.expect("the commit ends");
        })
        .expect("the run after the first cut ends");
        check_no_holder_after_a_cut(sim, &node);
    }

    /// The close of `subject-a` finds no room, so its handoff waits across the shed and
    /// the carry, and the ring's last record names `subject-a`. Its reopen gives
    /// control back to the recorded holder, so nothing is appended.
    #[test]
    fn keeps_a_handoff_that_waits_for_room_across_a_shed_and_a_carry() {
        run(186, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            // A sealed record, so the handoff of the close needs a block.
            shard.committed().await.expect("the commit ends");
            let blocks = test.fill();
            shard.close_writer(a);
            shard.shed(Slot::new(0));
            shard.shed(Slot::new(2));
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            drop(blocks);
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-a"));
            assert_eq!(handoffs.len(), 2, "one handoff to subject-a for each index");
        });
    }

    #[test]
    fn gives_no_deadline_when_no_holder_has_a_lease() {
        run(230, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            assert_eq!(shard.deadline(), None);
            shard.open_writer(writer("a", 1, &set)).expect("synced");
            assert_eq!(shard.deadline(), None);
        });
    }

    #[test]
    fn gives_the_end_of_the_earliest_lease_as_the_deadline() {
        run(231, |test| async move {
            let index = |slot| {
                intern(&[Group {
                    index: key(Slot::new(slot)),
                    data: &[],
                }])
            };
            let mut shard = test.shard(AREA).await;
            let opened = test.reader.now().monotonic;
            let long = Writer {
                lease: Some(Span::from_nanos(2_000_000)),
                ..writer("a", 1, &index(0))
            };
            shard.open_writer(long).expect("synced");
            let short = Writer {
                lease: Some(LEASE),
                ..writer("b", 1, &index(2))
            };
            shard.open_writer(short).expect("synced");
            assert_eq!(shard.deadline(), Some(opened + LEASE));
        });
    }

    #[test]
    fn gives_the_end_of_the_grace_as_the_deadline_after_a_power_cut() {
        let (mut sim, node) = cut_after(232, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let mut shard = test.open(AREA, 4).await;
            let opened = test.reader.now().monotonic;
            assert_eq!(shard.deadline(), None, "no index is carried");
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert_eq!(shard.deadline(), Some(opened + GRACE));
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn advances_nothing_before_the_deadline() {
        run(233, |test| async move {
            let (mut shard, a, set) = test.leased().await;
            shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            let deadline = shard.deadline();
            shard.advance();
            assert_eq!(shard.deadline(), deadline);
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            assert_eq!(
                shard.write(a, LIVE, write),
                Ok(&[applied(0, 0, 1), applied(2, 0, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff_to("subject-b")), []);
        });
    }

    /// A handoff that found no room is not recorded at an advance with no deadline.
    #[test]
    fn appends_no_waiting_handoff_at_an_advance_with_no_deadline() {
        run(240, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let blocks = test.fill();
            shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            drop(blocks);
            assert_eq!(shard.deadline(), None);
            shard.advance();
            shard.committed().await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff_to("subject-b")), []);
        });
    }

    /// A handoff that found no room is recorded at the next open of a writer on its
    /// index.
    #[test]
    fn records_a_waiting_handoff_at_the_open_of_another_writer() {
        run(244, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let blocks = test.fill();
            shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            drop(blocks);
            shard
                .open_writer(writer("subject-c", 1, &set))
                .expect("synced");
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 2, "the open of c records each index of b");
        });
    }

    /// A handoff that found no room is recorded at the next close of a writer on its
    /// index.
    #[test]
    fn records_a_waiting_handoff_at_the_close_of_another_writer() {
        run(245, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let c = shard
                .open_writer(writer("subject-c", 1, &set))
                .expect("synced");
            let blocks = test.fill();
            shard
                .open_writer(writer("subject-b", 2, &set))
                .expect("synced");
            drop(blocks);
            shard.close_writer(c);
            shard.committed().await.expect("the commit ends");
            let to_b = [&[2], "subject-b".as_bytes()].concat();
            let handoffs = find(&test.ring().await, &to_b);
            assert_eq!(handoffs.len(), 2, "the close of c records each index of b");
        });
    }

    /// A handoff that found no room is not recorded at a resend write: the shard does
    /// not read a resend frame. The next write that is not a resend still records it.
    #[test]
    fn keeps_a_waiting_handoff_at_a_resend_write() {
        run(246, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let blocks = test.fill();
            let b = shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            drop(blocks);
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(b, Label::Resend, write), Err(Error::Resend));
            shard.committed().await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff_to("subject-b")), []);
            let write = frame(&test.pool, &set, &[(2, &[11])]);
            shard.write(b, LIVE, write).expect("written");
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 1, "the live write records the handoff");
        });
    }

    /// A handoff that found no room waits, also when the end of a lease of its writer
    /// on another index records a handoff.
    #[test]
    fn keeps_a_waiting_handoff_at_the_end_of_a_lease_on_another_index() {
        run(241, |test| async move {
            let index = intern(&[Group {
                index: key(Slot::new(2)),
                data: &[],
            }]);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let c = shard.open_writer(writer("c", 1, &index)).expect("synced");
            let leased = Writer {
                lease: Some(LEASE),
                ..writer("subject-b", 1, &set)
            };
            shard.open_writer(leased).expect("synced");
            test.clock.sleep(WAIT).await;
            let blocks = test.fill();
            shard.close_writer(c);
            drop(blocks);
            test.clock.sleep(Span::from_nanos(400_000)).await;
            shard.advance();
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 1, "only the open recorded a handoff to b");
        });
    }

    /// The handoff that an end of a lease on another index keeps waiting is recorded at
    /// the next frame of its index.
    #[test]
    fn records_the_kept_handoff_at_the_next_frame_of_its_index() {
        run(243, |test| async move {
            let index = intern(&[Group {
                index: key(Slot::new(2)),
                data: &[],
            }]);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let c = shard.open_writer(writer("c", 1, &index)).expect("synced");
            let leased = Writer {
                lease: Some(LEASE),
                ..writer("subject-b", 1, &set)
            };
            let b = shard.open_writer(leased).expect("synced");
            test.clock.sleep(WAIT).await;
            let blocks = test.fill();
            shard.close_writer(c);
            drop(blocks);
            test.clock.sleep(Span::from_nanos(400_000)).await;
            shard.advance();
            let write = frame(&test.pool, &set, &[(2, &[10])]);
            assert_eq!(shard.write(b, LIVE, write), Ok(&[applied(2, 0, 1)][..]));
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(
                handoffs.len(),
                2,
                "the frame on slot 2 records the kept one"
            );
        });
    }

    /// A handoff at the end of a lease that finds no room is recorded at the next frame
    /// of its index.
    #[test]
    fn keeps_the_handoff_of_a_lease_end_with_no_room_for_the_next_frame() {
        run(242, |test| async move {
            let (mut shard, _, set) = test.leased().await;
            let b = shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            test.clock.sleep(WAIT).await;
            let blocks = test.fill();
            shard.advance();
            drop(blocks);
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(b, LIVE, write), Ok(&[applied(0, 0, 1)][..]));
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 1, "the frame on slot 0 records its handoff");
        });
    }

    /// `advance` drops each restored holder whose grace ended, so it leaves no
    /// deadline that came due.
    #[test]
    fn advances_past_the_end_of_the_grace_before_the_node_has_mesh_time() {
        let (mut sim, node) = cut_after(237, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let mut shard = test.unsynced().await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            test.clock.sleep(GRACE).await;
            assert!(shard.deadline().is_some_and(|due| due <= test.clock.now()));
            shard.advance();
            assert_eq!(shard.deadline(), None);
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn keeps_a_handoff_that_comes_due_before_the_node_has_mesh_time_unrecorded() {
        let (mut sim, node) = cut_after(189, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            let (clock, reader) = clock::Clock::new(test.clock.clone());
            let mut shard = Test::with(0, buffer, reader).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            test.clock.sleep(GRACE).await;
            shard.advance();
            let wall = test.node.wall();
            test.tasks.spawn(async move { clock.run(wall).await });
            while shard.now().is_none() {
                test.clock.sleep(Span::from_nanos(1)).await;
            }
            // The drop to no holder is not on disk, so a reopen of a changes nothing.
            shard.open_writer(writer("a", 1, &set)).expect("synced");
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("a"));
            assert_eq!(handoffs.len(), 2, "only the handoffs before the cut");
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn starts_a_carry_again_only_after_a_shed_that_keeps_no_gate() {
        run(192, |test| async move {
            let mut shard = test.shard(AREA).await;
            assert!(!shard.carry(Slot::new(0), key(Slot::new(0))));
            shard.shed(Slot::new(0));
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(!shard.carry(Slot::new(0), key(Slot::new(0))));
        });
    }

    #[test]
    fn starts_a_carry_again_only_after_a_shed_that_keeps_a_gate() {
        let (mut sim, node) = cut_after(193, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let mut shard = test.shard(AREA).await;
            assert!(!shard.carry(Slot::new(0), key(Slot::new(0))));
            shard.shed(Slot::new(2));
            shard.shed(Slot::new(0));
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.deadline().is_some(), "the shed kept the gate");
            assert!(!shard.carry(Slot::new(0), key(Slot::new(0))));
        })
        .expect("the run after the cut ends");
    }

    /// A kept gate that equals a new one gives the next carry the same control, so no
    /// public call shows what a shed keeps, and the test reads `kept`. Only it fails
    /// when the shed keeps each gate.
    #[test]
    fn keeps_no_gate_at_a_shed_of_an_index_with_no_holder_and_no_waiting_handoff() {
        run(190, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            shard.close_writer(a);
            shard.shed(Slot::new(0));
            shard.shed(Slot::new(2));
            assert!(shard.kept.is_empty(), "each gate equals a new one");
        });
    }

    #[test]
    fn advances_past_the_end_of_each_grace_with_no_open_writer() {
        let (mut sim, node) = cut_after(237, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let mut shard = test.shard(AREA).await;
            test.clock.sleep(GRACE).await;
            assert!(shard.deadline().is_some_and(|due| due <= test.clock.now()));
            shard.advance();
            assert_eq!(shard.deadline(), None);
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn records_the_handoff_to_a_waiting_writer_at_the_end_of_a_lease_in_index_order() {
        run(234, |test| async move {
            let (mut shard, _, set) = test.leased().await;
            let leased = Writer {
                lease: Some(LEASE),
                ..writer("subject-b", 1, &set)
            };
            shard.open_writer(leased).expect("synced");
            test.clock.sleep(WAIT).await;
            let now = test.reader.now().monotonic;
            assert!(shard.deadline().is_some_and(|due| due <= now));
            let marked = test.now();
            shard.advance();
            assert!(shard.deadline() > Some(now), "b's lease ends later");
            shard.committed().await.expect("the commit ends");
            let ring = test.ring().await;
            assert_eq!(find(&ring, &handoff_to("subject-b")).len(), 2);
            let zero = key(Slot::new(0)).as_u128();
            let two = key(Slot::new(2)).as_u128();
            assert_eq!(
                headers(&ring, marked),
                [(zero, 0, 0, 0, 1), (two, 0, 0, 0, 1)]
            );
        });
    }

    #[test]
    fn records_the_handoff_to_a_waiting_writer_at_the_end_of_the_grace() {
        let (mut sim, node) = cut_after(236, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            test.clock.sleep(GRACE).await;
            shard.advance();
            shard.committed().await.expect("the commit ends");
            let handoffs = find(&test.ring().await, &handoff_to("subject-b"));
            assert_eq!(handoffs.len(), 2, "b takes both indexes");
        })
        .expect("the run after the cut ends");
    }

    /// With no record of the end of the lease, the open after the cut restores `a`
    /// and reserves control for it.
    #[test]
    fn records_no_holder_at_the_end_of_a_lease_before_a_power_cut() {
        let (mut sim, node) = create_node(235);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let (mut shard, a, set) = test.leased().await;
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            assert_eq!(
                shard.write(a, LIVE, write),
                Ok(&[applied(0, 0, 1), applied(2, 0, 1)][..])
            );
            test.clock.sleep(LEASE).await;
            shard.advance();
            shard.committed().await.expect("the commit ends");
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            assert_eq!(shard.deadline(), None, "no holder is restored");
            let b = shard
                .open_writer(writer("subject-b", 1, &set))
                .expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn records_no_handoff_when_the_holder_reopens_after_the_grace() {
        let (mut sim, node) = create_node(177);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            shard.committed().await.expect("the commit ends");
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.open(AREA, 4).await;
            let before = find(&test.ring().await, &handoff_to("subject-a")).len();
            test.clock.sleep(GRACE).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            shard.committed().await.expect("the commit ends");
            let after = find(&test.ring().await, &handoff_to("subject-a")).len();
            assert_eq!((before, after), (2, 2));
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn open_gives_the_error_of_a_failed_read_of_a_handoff() {
        let (mut sim, node) = cut_after(182, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            test.node.fail_file(FilePath::new(RING), Operation::ReadAt);
            let config = Config {
                shard: 0,
                buffer,
                clock: test.reader.clone(),
                limits: LIMITS,
            };
            let failed = env::files::Error::Io {
                path: PathBuf::from(RING),
                operation: Operation::ReadAt,
                code: 5,
            };
            assert_eq!(
                Shard::open(config).await.err(),
                Some(buffer::Error::Files(failed))
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn restores_control_with_no_mesh_time_after_a_power_cut() {
        let (mut sim, node) = cut_after(176, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            let (clock, reader) = clock::Clock::new(test.clock.clone());
            let mut shard = Test::with(0, buffer, reader).await;
            assert_eq!(shard.now(), None);
            let wall = test.node.wall();
            test.tasks.spawn(async move { clock.run(wall).await });
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            while shard.now().is_none() {
                test.clock.sleep(Span::from_nanos(1)).await;
            }
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[refused(0, Refusal::Reserved), refused(2, Refusal::Reserved)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn open_panics_on_a_handoff_record_that_does_not_decode() {
        let (mut sim, node) = create_node(175);
        let ended = sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            let body = test.pool.copy(&[1, 0xff]).expect("the pool has a block");
            let record = Entry {
                index: key(Slot::new(0)),
                slot: Slot::new(0),
                path: Path::Live,
                first: 0,
                samples: None,
                stored_at: Stamp::from_nanos(1),
                tag: 1,
                parts: Some(body).into(),
            };
            buffer.append([record]).expect("the ring has room");
            buffer.committed().await.expect("the commit ends");
            test.over(buffer).await;
        });
        let Err(sim::Error::Panicked {
            thread, message, ..
        }) = ended
        else {
            panic!("the open panics: {ended:?}");
        };
        assert_eq!(thread, "run_on");
        let invalid = "invalid utf-8 sequence of 1 bytes from index 0";
        assert_eq!(
            message,
            format!("the handoff subject is not UTF-8: {invalid}")
        );
    }

    #[test]
    fn reserves_control_until_the_grace_ends_after_a_power_cut() {
        let (mut sim, node) = cut_after(180, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            test.clock.sleep(Span::from_nanos(9_999_999_999)).await;
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[refused(0, Refusal::Reserved), refused(2, Refusal::Reserved)][..])
            );
            test.clock.sleep(Span::from_nanos(1)).await;
            let next = frame(&test.pool, &set, &[(0, &[30]), (1, &[3]), (2, &[30])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn restores_the_waiter_that_took_control_from_a_reopened_holder_after_a_power_cut()
    {
        let (mut sim, node) = cut_after(181, false);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            shard.close_writer(a);
            let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[2]), (2, &[20])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 1, 1), applied(2, 1, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
        })
        .expect("the second run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let c = shard.open_writer(writer("c", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[30]), (1, &[3]), (2, &[30])]);
            assert_eq!(
                shard.write(c, LIVE, next),
                Ok(&[refused(0, Refusal::Reserved), refused(2, Refusal::Reserved)][..])
            );
            let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[40]), (1, &[4]), (2, &[40])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 2, 1), applied(2, 2, 1)][..])
            );
        })
        .expect("the third run ends");
    }

    #[test]
    fn continues_at_a_lost_range_after_a_power_cut_after_an_empty_group() {
        let (mut sim, node) = create_node(120);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            let gone = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, gone), Ok(&[lost(0, 1, 1)][..]));
            drop(blocks);
            let empty = frame(&test.pool, &set, &[(0, &[]), (1, &[])]);
            assert_eq!(shard.write(a, LIVE, empty), Ok(&[applied(0, 2, 0)][..]));
            shard.committed().await.expect("the commit ends");
            assert_eq!(stored(&shard, Slot::new(0), Path::Live), 1);
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[40]), (1, &[4])]);
            assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(0, 1, 1)][..]));
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn skips_a_lost_range_after_a_power_cut_after_an_empty_write_records_a_handoff() {
        let (mut sim, node) = create_node(121);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            let gone = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, gone), Ok(&[lost(0, 1, 1)][..]));
            // The lost frame gave its block back.
            let more = test.fill();
            // b outranks a; its handoff finds no block and waits.
            let b = shard.open_writer(writer("b", 2, &set)).expect("synced");
            shard.committed().await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &[2, b'b']).len(), 0, "b waits");
            assert_eq!(stored(&shard, Slot::new(0), Path::Live), 1);
            drop((blocks, more));
            // The empty group records the waiting handoff and stores no sample.
            let empty = frame(&test.pool, &set, &[(0, &[]), (1, &[])]);
            assert_eq!(shard.write(b, LIVE, empty), Ok(&[applied(0, 2, 0)][..]));
            shard.committed().await.expect("the commit ends");
            assert_eq!(
                find(&test.ring().await, &[2, b'b']).len(),
                1,
                "the empty write recorded the handoff to b"
            );
            assert_eq!(stored(&shard, Slot::new(0), Path::Live), 2);
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let b = shard.open_writer(writer("b", 2, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[40]), (1, &[4])]);
            assert_eq!(
                shard.write(b, LIVE, next),
                Ok(&[applied(0, 2, 1)][..]),
                "the handoff on disk made the lost range durable"
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn continues_at_a_lost_range_after_a_power_cut_after_a_backfill_entry() {
        let (mut sim, node) = create_node(122);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            let gone = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, gone), Ok(&[lost(0, 1, 1)][..]));
            drop(blocks);
            // A backfill entry of index 0 with samples goes to disk.
            let later = frame(&test.pool, &set, &[(0, &[3, 4, 5]), (1, &[3, 4, 5])]);
            assert_eq!(shard.write(a, BACKFILL, later), Ok(&[applied(0, 0, 3)][..]));
            shard.committed().await.expect("the commit ends");
        })
        .expect("the first run ends");
        sim.crash(&node, sim::Crash::Power);
        sim.run_on(&node, |node, tasks| async move {
            let test = Test::new(node, tasks);
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let next = frame(&test.pool, &set, &[(0, &[40]), (1, &[4])]);
            assert_eq!(
                shard.write(a, LIVE, next),
                Ok(&[applied(0, 1, 1)][..]),
                "only a live entry moves the live stored mark"
            );
        })
        .expect("the run after the cut ends");
    }

    #[test]
    fn continues_each_path_after_a_power_cut_after_a_commit() {
        let (mut sim, node) = create_node(62);
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
        let (mut sim, node) = create_node(63);
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
            let buffer = test.create_buffer(AREA, 4087, 1).await;
            let mut shard = test.over(buffer).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            let set = intern(&[Group {
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
            let set = intern(&[Group {
                index: key(Slot::new(2)),
                data: &[(key(Slot::new(1)), Type::Scalar(Scalar::I64))],
            }]);
            let mut shard = test.open(AREA, 4).await;
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
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
    fn stores_no_entry_for_a_group_with_no_samples() {
        run(117, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            shard.write(a, LIVE, first).expect("written");
            test.clock.sleep(Span::from_nanos(1)).await;
            let marked = test.now();
            let write = frame(&test.pool, &set, &[(0, &[]), (1, &[]), (2, &[20])]);
            assert_eq!(
                shard.write(a, LIVE, write),
                Ok(&[applied(0, 1, 0), applied(2, 1, 1)][..])
            );
            shard.committed().await.expect("the commit ends");
            let two = key(Slot::new(2)).as_u128();
            assert_eq!(headers(&test.ring().await, marked), [(two, 0, 1, 1, 0)]);
        });
    }

    #[test]
    fn applies_a_write_with_no_samples_whose_handoff_has_no_room() {
        run(118, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let empty = frame(&test.pool, &set, &[(0, &[]), (1, &[])]);
            let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            let blocks = test.fill();
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            assert_eq!(shard.write(a, LIVE, empty), Ok(&[applied(0, 0, 0)][..]));
            drop(blocks);
            assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(0, 0, 1)][..]));
            shard.committed().await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff_to("subject-a")).len(), 1);
        });
    }

    #[test]
    fn refuses_a_backfill_write_with_no_samples_whose_handoff_has_no_room() {
        run(119, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let empty = frame(&test.pool, &set, &[(0, &[]), (1, &[])]);
            let blocks = test.fill();
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            assert_eq!(shard.write(a, BACKFILL, empty), Err(Error::Full));
            drop(blocks);
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
                    let mut out = Vec::new();
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
            shard.open_latest(slot).key
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

        /// The mark of the last read is at seq 4. The session takes seq 4 live, spends
        /// its credit, and misses seqs 5 and 6: it reads them from seq 5, not from the
        /// old mark.
        #[test]
        fn reads_from_the_seq_after_its_last_frame_and_not_from_an_old_mark() {
            run(140, |test| async move {
                let mut shard = test.shard(AREA).await;
                let set = two_indexes();
                let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                let commit = async |shard: &mut Shard, stamps: &[i64]| {
                    for &stamp in stamps {
                        write(&test, shard, a, &[stamp]);
                    }
                    shard.committed().await.expect("the commit ends");
                    woken(shard);
                };
                commit(&mut shard, &[1, 2]).await;
                commit(&mut shard, &[3, 4]).await;
                // Each frame charges 192 bytes: seq 4 spends the last of the credit.
                shard.grant(session, 800);
                let (frames, _, reads) = read(&mut shard, session).await;
                let caught_up: Vec<_> = (0..4).map(|n| seq(n, 1)).collect();
                assert_eq!((ranges(&frames, 0), reads), (caught_up, 1));
                commit(&mut shard, &[5]).await;
                assert_eq!(taken(&mut shard, session.into(), 0), [seq(4, 1)]);
                commit(&mut shard, &[6]).await;
                commit(&mut shard, &[7]).await;
                shard.grant(session, 1600);
                let (frames, end, reads) = read(&mut shard, session).await;
                assert_eq!(
                    (ranges(&frames, 0), reads),
                    (vec![seq(5, 1), seq(6, 1)], 1)
                );
                assert!(matches!(end, reader::Next::Empty), "{end:?}");
            });
        }

        #[test]
        fn opens_and_closes_a_reader_of_each_mode_with_no_mesh_time() {
            run(97, |test| async move {
                let mut shard = test.unsynced().await;
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                let readers = [session.into(), shard.open_latest(Slot::new(0)).key];
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
                let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
                let (clock, mesh) = clock::Clock::new(test.clock.clone());
                let mut shard = Test::with(0, buffer, mesh.clone()).await;
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
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
        fn gives_a_complete_reader_the_frames_on_each_side_of_a_lost_group() {
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
        fn gives_a_complete_reader_a_frame_that_waits_for_credit_at_a_grant() {
            run(37, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                let reader = reader::Key::from(session);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                write(&test, &mut shard, a, &[20]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                shard.grant(session, CREDIT);
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1)]);
            });
        }

        #[test]
        fn misses_a_grant_reads_disk_and_goes_live_with_no_loss() {
            run(37, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                let reader = reader::Key::from(session);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                write(&test, &mut shard, a, &[20]);
                assert_eq!(taken(&mut shard, reader, 0), []);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                write(&test, &mut shard, a, &[30]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(read(&mut shard, reader).await.2, 0, "no credit");
                shard.grant(session, CREDIT);
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                let frames = caught_up(&mut shard, session, 0).await;
                assert_eq!(frames, [seq(1, 1), seq(2, 1), seq(3, 1)]);
                write(&test, &mut shard, a, &[50]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(4, 1)]);
            });
        }

        /// The session takes seq 0, spends its credit, and misses the later frames
        /// while the ring trims them. Its read from disk gives the frames from the
        /// first seq the ring holds, with no frame for the trimmed seqs between.
        #[test]
        fn gives_a_trimmed_range_as_a_jump_in_seq() {
            run(7041, |test| async move {
                let test = Test {
                    trimmed: true,
                    ..test
                };
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                let reader = reader::Key::from(session);
                let set = two_indexes();
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                let mut wakes = Vec::new();
                for stamp in 2..=300 {
                    write(&test, &mut shard, a, &[stamp * 10]);
                    shard.committed().await.expect("the commit ends");
                    wakes.extend(woken(&mut shard));
                }
                assert_eq!(wakes, [reader]);
                // The buffer of the shard gives the first seq that the ring holds: no
                // call of `Shard` gives it.
                let ring = shard.buffer.reader();
                let read = ring.read(Slot::new(0), Path::Live, Mark::at(1), 1).await;
                let gap = read.expect("the read ends").gap.expect("a trim");
                assert_eq!(gap.start, 1);
                shard.grant(session, CREDIT);
                let frames = caught_up(&mut shard, session, 0).await;
                let held: Vec<_> = (gap.end..300).map(|s| seq(s, 1)).collect();
                assert_eq!(frames, held);
            });
        }

        #[test]
        fn gives_a_frame_that_waits_for_credit_on_a_quiet_index_after_a_grant() {
            run(39, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                let reader = reader::Key::from(session);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                for stamp in [10, 20, 30] {
                    write(&test, &mut shard, a, &[stamp]);
                }
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                let other = frame(&test.pool, &set, &[(2, &[40])]);
                assert_eq!(shard.write(a, LIVE, other), Ok(&[applied(2, 0, 1)][..]));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                shard.grant(session, CREDIT);
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1), seq(2, 1)]);
            });
        }

        #[test]
        fn charges_a_complete_reader_of_places_the_frame_of_its_places() {
            run(38, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let stamps =
                    |n: i64| -> Vec<_> { (n * 100 + 1..=(n + 1) * 100).collect() };
                let probe = complete(&mut shard, Slot::new(0));
                write(&test, &mut shard, a, &stamps(0));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [probe]);
                let reader::Next::Frame(probed) = shard.take(probe) else {
                    panic!("a frame waits");
                };
                let (_, index) = probed.ends().next().expect("the index is present");
                let index = types::frame::charge(1, index);
                assert!(probed.charge() > index + 1);
                let places = Charge::Places([Slot::new(0)].into());
                let session = shard.open_complete(Slot::new(0), index + 1, places).key;
                let reader = reader::Key::from(session);
                let places = Charge::Places([Slot::new(1), Slot::new(0)].into());
                let data = shard.open_complete(Slot::new(0), 2 * index + 1, places).key;
                for n in 1..4 {
                    write(&test, &mut shard, a, &stamps(n));
                }
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [probe, reader, data.into()]);
                for key in [reader, data.into()] {
                    assert_eq!(
                        taken(&mut shard, key, 0),
                        [seq(100, 100), seq(200, 100)]
                    );
                }
                write(&test, &mut shard, a, &stamps(4));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader, data.into()]);
                for session in [session, data] {
                    assert_eq!(taken(&mut shard, session.into(), 0), []);
                    shard.grant(session, CREDIT);
                    assert_eq!(
                        caught_up(&mut shard, session, 0).await,
                        [seq(300, 100), seq(400, 100)]
                    );
                }
            });
        }

        #[test]
        fn names_a_complete_reader_once_when_it_misses_a_frame_with_none_waiting() {
            run(110, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard
                    .open_complete(Slot::new(0), 1, Charge::Whole)
                    .key
                    .into();
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                write(&test, &mut shard, a, &[20]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), []);
                write(&test, &mut shard, a, &[30]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), []);
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), []);
            });
        }

        #[test]
        fn does_not_name_a_complete_reader_that_misses_a_frame_while_one_waits() {
            run(111, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard
                    .open_complete(Slot::new(0), 1, Charge::Whole)
                    .key
                    .into();
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                for stamp in [20, 30] {
                    write(&test, &mut shard, a, &[stamp]);
                    shard.committed().await.expect("the commit ends");
                    assert_eq!(woken(&mut shard), []);
                }
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(woken(&mut shard), []);
            });
        }

        #[test]
        fn does_not_count_a_frame_of_no_samples_as_a_miss_of_a_complete_reader() {
            run(104, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(2), 1, Charge::Whole).key;
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
        fn keeps_the_newest_frame_with_samples_after_an_empty_live_write() {
            run(115, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                let empty = frame(&test.pool, &set, &[(0, &[]), (1, &[])]);
                assert_eq!(shard.write(a, LIVE, empty), Ok(&[applied(0, 1, 0)][..]));
                let reader = latest(&mut shard, Slot::new(0));
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
            });
        }

        #[test]
        fn applies_an_empty_group_of_a_live_write_whose_other_group_is_lost() {
            run(116, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                let mixed = frame(&test.pool, &set, &[(0, &[]), (1, &[]), (2, &[20])]);
                let blocks = test.fill();
                assert_eq!(
                    shard.write(a, LIVE, mixed),
                    Ok(&[applied(0, 1, 0), super::lost(2, 0, 1)][..])
                );
                drop(blocks);
                let reader = latest(&mut shard, Slot::new(0));
                assert_eq!(woken(&mut shard), []);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
            });
        }

        #[test]
        fn raises_the_credit_of_a_complete_reader_with_a_grant() {
            run(25, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
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
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                close(&mut shard, session.into());
                shard.grant(session, CREDIT);
                let after = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                assert_ne!(after, session);
            });
        }

        #[test]
        fn gives_the_live_tail_as_the_start_of_a_complete_reader() {
            run(247, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10, 20]);
                let opened = shard.open_complete(Slot::new(0), 1, Charge::Whole);
                assert_eq!(opened.live, 2);
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
                let _key = shard.open_latest(Slot::new(3)).key;
            });
        }

        #[test]
        fn panics_at_the_open_of_a_complete_reader_of_an_index_it_does_not_carry() {
            check_not_carried(99, |shard| {
                let _key = shard.open_complete(Slot::new(3), CREDIT, Charge::Whole).key;
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
                let reader =
                    shard.open_complete(Slot::new(2), CREDIT, Charge::Whole).key;
                let other = reader::complete::Key {
                    slot: Slot::new(3),
                    ..reader
                };
                shard.grant(other, CREDIT + 1);
            });
        }

        /// The key set of the index at slot 2 alone.
        fn only_two() -> Arc<KeySet> {
            intern(&[Group {
                index: key(Slot::new(2)),
                data: &[],
            }])
        }

        /// Shed at place 0 moves the index at slot 2 there, with a frame that waits
        /// for its reader.
        #[test]
        fn keeps_the_writer_and_the_reader_of_an_index_that_a_shed_moves() {
            run(131, |test| async move {
                let set = only_two();
                let mut shard = test.shard(AREA).await;
                let reader = complete(&mut shard, Slot::new(2));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let first = frame(&test.pool, &set, &[(0, &[10])]);
                assert_eq!(shard.write(a, LIVE, first), Ok(&[applied(2, 0, 1)][..]));
                shard.committed().await.expect("the commit ends");
                shard.shed(Slot::new(0));
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                let next = frame(&test.pool, &set, &[(0, &[20])]);
                assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(2, 1, 1)][..]));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(1, 1)]);
            });
        }

        #[test]
        fn continues_the_seq_and_the_reader_keys_of_an_index_it_carries_again() {
            run(132, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let old = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10, 20]);
                shard.close_writer(a);
                shard.close_reader(old.into());
                shard.shed(Slot::new(0));
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                let new = shard.open_complete(Slot::new(0), 1, Charge::Whole).key;
                assert_ne!(reader::Key::from(new), reader::Key::from(old));
                shard.grant(old, CREDIT);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let next = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
                assert_eq!(shard.write(a, LIVE, next), Ok(&[applied(0, 2, 1)][..]));
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [new.into()]);
                assert_eq!(taken(&mut shard, new.into(), 0), [seq(2, 1)]);
            });
        }

        /// The key set of the indexes at slots 1 and 2.
        fn one_and_two() -> Arc<KeySet> {
            let groups = [1, 2].map(|slot| Group {
                index: key(Slot::new(slot)),
                data: &[],
            });
            intern(&groups)
        }

        /// Shed at place 0 moves the index at place 2 there, and not the one at
        /// place 1.
        #[test]
        fn keeps_the_writer_on_an_index_that_a_shed_does_not_move() {
            run(139, |test| async move {
                let (mut shard, _) = test.wide(3).await;
                let set = one_and_two();
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[10])]);
                let written = shard.write(a, LIVE, first);
                assert_eq!(written, Ok(&[applied(1, 0, 1), applied(2, 0, 1)][..]));
                shard.shed(Slot::new(0));
                let next = frame(&test.pool, &set, &[(0, &[20]), (1, &[20])]);
                let written = shard.write(a, LIVE, next);
                assert_eq!(written, Ok(&[applied(1, 1, 1), applied(2, 1, 1)][..]));
            });
        }

        #[test]
        fn wakes_the_readers_of_each_index_after_a_shed_moves_one() {
            run(140, |test| async move {
                let (mut shard, _) = test.wide(3).await;
                let set = one_and_two();
                let one = complete(&mut shard, Slot::new(1));
                let two = complete(&mut shard, Slot::new(2));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let first = frame(&test.pool, &set, &[(0, &[10]), (1, &[10])]);
                shard.write(a, LIVE, first).expect("written");
                shard.committed().await.expect("the commit ends");
                shard.shed(Slot::new(0));
                assert_eq!(woken(&mut shard), [one, two]);
            });
        }

        #[test]
        fn wakes_no_reader_of_an_index_it_shed() {
            run(141, |test| async move {
                let set = only_two();
                let mut shard = test.shard(AREA).await;
                let reader = complete(&mut shard, Slot::new(2));
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let first = frame(&test.pool, &set, &[(0, &[10])]);
                shard.write(a, LIVE, first).expect("written");
                shard.committed().await.expect("the commit ends");
                shard.close_writer(a);
                shard.close_reader(reader);
                shard.shed(Slot::new(2));
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
            });
        }

        /// The second shed ends an index with fewer reader keys than the first.
        #[test]
        fn gives_no_reader_key_twice_after_two_sheds() {
            run(142, |test| async move {
                let mut shard = test.shard(AREA).await;
                let old = latest(&mut shard, Slot::new(0));
                close(&mut shard, old);
                shard.shed(Slot::new(0));
                shard.shed(Slot::new(2));
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                assert_ne!(latest(&mut shard, Slot::new(0)), old);
            });
        }

        #[test]
        fn panics_at_the_shed_of_an_index_with_a_writer_open() {
            let message = "a writer is open on the index at Slot(0)";
            check_panics(133, message, |shard| {
                let set = two_indexes();
                shard.open_writer(writer("a", 1, &set)).expect("synced");
                shard.shed(Slot::new(0));
            });
        }

        #[test]
        fn panics_at_the_shed_of_an_index_with_a_reader_open() {
            let message = "a reader of the index is open";
            check_panics(134, message, |shard| {
                let _reader = latest(shard, Slot::new(0));
                shard.shed(Slot::new(0));
            });
        }

        #[test]
        fn does_nothing_at_the_shed_of_an_index_it_does_not_carry() {
            run(135, |test| async move {
                let mut shard = test.shard(AREA).await;
                shard.shed(Slot::new(3));
                shard.shed(Slot::new(0));
                shard.shed(Slot::new(0));
                let carried = latest(&mut shard, Slot::new(2));
                close(&mut shard, carried);
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                let carried = latest(&mut shard, Slot::new(0));
                close(&mut shard, carried);
            });
        }

        #[test]
        fn panics_at_the_take_of_a_reader_of_an_index_it_shed() {
            let message = "the shard does not carry the index at Slot(2)";
            check_panics(136, message, |shard| {
                let reader = latest(shard, Slot::new(2));
                shard.close_reader(reader);
                shard.shed(Slot::new(2));
                drop(shard.take(reader));
            });
        }

        /// Slot 3 is carried for the first time after a shed of slot 0, which gave
        /// latest keys 0 and 1. The shard never gave latest key 0 on slot 3.
        #[test]
        fn panics_at_the_take_of_a_key_never_given_on_an_index_carried_after_a_shed() {
            let message = "latest session 0 was never open";
            check_panics(152, message, |shard| {
                let two = latest(shard, Slot::new(2));
                for _ in 0..2 {
                    let reader = latest(shard, Slot::new(0));
                    close(shard, reader);
                }
                shard.shed(Slot::new(0));
                assert!(shard.carry(Slot::new(3), key(Slot::new(3))));
                let never = reader::Key {
                    slot: Slot::new(3),
                    session: two.session,
                };
                drop(shard.take(never));
            });
        }

        #[test]
        fn closes_the_keys_of_each_earlier_carry_of_an_index_shed_two_times() {
            run(153, |test| async move {
                let mut shard = test.shard(AREA).await;
                let mut old = Vec::new();
                for _ in 0..2 {
                    let reader = latest(&mut shard, Slot::new(0));
                    close(&mut shard, reader);
                    old.push(reader);
                    shard.shed(Slot::new(0));
                    assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                }
                for &reader in &old {
                    assert!(matches!(shard.take(reader), reader::Next::Empty));
                }
                let new = latest(&mut shard, Slot::new(0));
                assert!(!old.contains(&new), "{new:?} names an earlier reader");
            });
        }

        /// The ring is full, so the handoff of the close waits, and the shed keeps it.
        /// No entry is appended after the lost frames, so their gap goes.
        #[test]
        fn gives_the_first_lost_seq_again_after_a_shed_and_a_carry_as_after_a_restart()
        {
            run(137, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(1 << 16).await;
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let mut n = 0_i64;
                let lost_at = loop {
                    let stamps: Vec<i64> = (0..400).map(|k| 10 + n * 400 + k).collect();
                    let values = scattered(400);
                    let write = frame(&test.pool, &set, &[(0, &stamps), (1, &values)]);
                    let written =
                        shard.write(a, LIVE, write).expect("written").to_vec();
                    shard.committed().await.expect("the commit ends");
                    n += 1;
                    if let [Outcome::Lost { range, .. }] = written[..] {
                        break range.seq;
                    }
                };
                shard.close_writer(a);
                // No public outcome of this test shows the waiting handoff: the open
                // of b replaces it.
                assert!(shard.indexes[0].handoff().is_some(), "the handoff waits");
                shard.shed(Slot::new(0));
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
                let stamp = 10 + n * 400;
                let next = frame(&test.pool, &set, &[(0, &[stamp]), (1, &[3])]);
                let written = shard.write(b, LIVE, next).expect("written").to_vec();
                assert_eq!(written, [lost(0, lost_at, 1)]);
            });
        }

        /// A latest reader got seq 0 at stamp 20 as a lost frame. After a shed and a
        /// carry, the live path stands before it.
        #[test]
        fn applies_a_frame_at_a_lost_seq_after_a_shed_and_a_carry_as_after_a_restart() {
            run(138, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = latest(&mut shard, Slot::new(0));
                let gone = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
                let room = test.pool.alloc(80).expect("a block");
                let blocks = test.fill();
                // The handoff to a finds no block and waits, so the frame is lost.
                let subject = "a".repeat(200);
                let a = shard
                    .open_writer(writer(&subject, 1, &set))
                    .expect("synced");
                drop(room);
                assert_eq!(shard.write(a, LIVE, gone), Ok(&[lost(0, 0, 1)][..]));
                assert_eq!(woken(&mut shard), [reader]);
                assert_eq!(taken(&mut shard, reader, 0), [seq(0, 1)]);
                // No holder was ever recorded, so the close records nothing.
                shard.close_writer(a);
                shard.close_reader(reader);
                drop(blocks);
                shard.shed(Slot::new(0));
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
                let late = frame(&test.pool, &set, &[(0, &[15]), (1, &[3])]);
                assert_eq!(shard.write(b, LIVE, late), Ok(&[applied(0, 0, 1)][..]));
                let next = frame(&test.pool, &set, &[(0, &[25]), (1, &[3])]);
                assert_eq!(shard.write(b, LIVE, next), Ok(&[applied(0, 1, 1)][..]));
            });
        }

        /// The tail in the buffer holds an entry appended before the shed, also one not
        /// yet on disk, so the lost seq before it is not given again.
        #[test]
        fn continues_after_a_later_entry_not_yet_on_disk_after_a_shed_and_a_carry() {
            run(150, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let gone = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
                let again = frame(&test.pool, &set, &[(0, &[30]), (1, &[3])]);
                let room = test.pool.alloc(80).expect("a block");
                let blocks = test.fill();
                let subject = "a".repeat(200);
                let a = shard
                    .open_writer(writer(&subject, 1, &set))
                    .expect("synced");
                drop(room);
                assert_eq!(shard.write(a, LIVE, gone), Ok(&[lost(0, 0, 1)][..]));
                drop(blocks);
                assert_eq!(shard.write(a, LIVE, again), Ok(&[applied(0, 1, 1)][..]));
                shard.close_writer(a);
                assert_eq!(stored(&shard, Slot::new(0), Path::Live), 0);
                shard.shed(Slot::new(0));
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
                let next = frame(&test.pool, &set, &[(0, &[40]), (1, &[4])]);
                assert_eq!(shard.write(b, LIVE, next), Ok(&[applied(0, 2, 1)][..]));
            });
        }

        /// The handoff of the close is the only entry after the lost seq, and it is not
        /// yet on disk, so the lost seq is not given again.
        #[test]
        fn continues_after_a_later_handoff_not_yet_on_disk_after_a_shed_and_a_carry() {
            run(151, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let gone = frame(&test.pool, &set, &[(0, &[20]), (1, &[2])]);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                let blocks = test.fill();
                assert_eq!(shard.write(a, LIVE, gone), Ok(&[lost(0, 0, 1)][..]));
                drop(blocks);
                shard.close_writer(a);
                assert_eq!(stored(&shard, Slot::new(0), Path::Live), 0);
                shard.shed(Slot::new(0));
                assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
                let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
                let next = frame(&test.pool, &set, &[(0, &[40]), (1, &[4])]);
                assert_eq!(shard.write(b, LIVE, next), Ok(&[applied(0, 1, 1)][..]));
            });
        }

        mod named {
            use super::*;
            use crate::reader::{
                Opened, Position, Unopened, Unsynced, complete, named,
            };

            const HOLD: Span = Span::from_nanos(1_000_000_000);
            /// One nanosecond less than `HOLD`.
            const INSIDE: Span = Span::from_nanos(999_999_999);

            /// Opens the named complete reader `name` of `subject` on the index at
            /// slot 0, with a hold of `HOLD` and a credit of `CREDIT`.
            fn named(
                shard: &mut Shard,
                subject: &str,
                name: &str,
            ) -> Opened<complete::Key> {
                shard
                    .open_named_complete(
                        Slot::new(0),
                        key(subject, name),
                        HOLD,
                        CREDIT,
                        Charge::Whole,
                    )
                    .expect("synced")
            }

            /// Opens the named latest reader `name` of `subject` on the index at
            /// slot 0.
            fn latest(
                shard: &mut Shard,
                subject: &str,
                name: &str,
            ) -> Opened<reader::Key> {
                shard
                    .open_named_latest(Slot::new(0), key(subject, name))
                    .expect("synced")
            }

            /// The key of the named reader `name` of `subject`.
            fn key(subject: &str, name: &str) -> named::Key {
                named::Key {
                    subject: subject.parse().expect("a valid name"),
                    name: name.parse().expect("a valid name"),
                }
            }

            fn live(live: u64) -> Position {
                Position {
                    live,
                    backfill: None,
                }
            }

            /// Writes one live frame of one sample for each of `stamps`, waits for the
            /// commit, and gives the readers to wake.
            async fn committed(
                test: &Test,
                shard: &mut Shard,
                a: writer::Key,
                stamps: &[i64],
            ) -> Vec<reader::Key> {
                for &stamp in stamps {
                    write(test, shard, a, &[stamp]);
                }
                shard.committed().await.expect("the commit ends");
                woken(shard)
            }

            #[test]
            fn opens_no_named_reader_before_the_node_has_mesh_time() {
                run(120, |test| async move {
                    let mut shard = test.unsynced().await;
                    assert!(shard.carry(Slot::new(0), common::key(Slot::new(0))));
                    let complete = shard.open_named_complete(
                        Slot::new(0),
                        key("s", "r"),
                        HOLD,
                        CREDIT,
                        Charge::Whole,
                    );
                    assert_eq!(complete, Err(Unopened::Unsynced));
                    assert_eq!(
                        shard.open_named_latest(Slot::new(0), key("s", "r")),
                        Err(Unsynced)
                    );
                });
            }

            #[test]
            fn opens_another_reader_for_the_same_name_of_another_subject() {
                run(121, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    assert_eq!(first.replaced, None);
                    committed(&test, &mut shard, a, &[10, 20]).await;
                    let frames = taken(&mut shard, first.key.into(), 0);
                    assert_eq!(frames, [seq(0, 1), seq(1, 1)]);
                    shard.ack(first.key, live(2)).expect("forward");
                    let other = named(&mut shard, "b", "r");
                    assert_eq!(other.replaced, None);
                    let woken = committed(&test, &mut shard, a, &[30]).await;
                    assert_eq!(woken, [first.key.into(), other.key.into()]);
                    assert_eq!(taken(&mut shard, first.key.into(), 0), [seq(2, 1)]);
                    assert_eq!(taken(&mut shard, other.key.into(), 0), [seq(2, 1)]);
                    shard.ack(first.key, live(3)).expect("forward");
                    assert_eq!(shard.ack(first.key, live(1)), Ok(()));
                    let again = named(&mut shard, "a", "r");
                    assert_eq!(taken(&mut shard, again.key.into(), 0), []);
                });
            }

            #[test]
            fn takes_over_the_session_of_the_same_subject_and_name() {
                run(122, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10]).await;
                    assert_eq!(taken(&mut shard, first.key.into(), 0), [seq(0, 1)]);
                    shard.ack(first.key, live(1)).expect("forward");
                    committed(&test, &mut shard, a, &[20]).await;
                    let second = named(&mut shard, "a", "r");
                    assert_eq!(second.replaced, Some(first.key.into()));
                    assert_eq!(woken(&mut shard), []);
                    assert_eq!(taken(&mut shard, first.key.into(), 0), []);
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1)]);
                    close(&mut shard, first.key.into());
                    close(&mut shard, second.key.into());
                });
            }

            #[test]
            fn gives_each_later_frame_after_an_open_again_at_its_last_ack() {
                run(123, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20]).await;
                    assert_eq!(taken(&mut shard, first.key.into(), 0).len(), 2);
                    shard.ack(first.key, live(2)).expect("forward");
                    close(&mut shard, first.key.into());
                    assert_eq!(shard.ack(first.key, live(1)), Ok(()));
                    let second = named(&mut shard, "a", "r");
                    assert_eq!(second.replaced, None);
                    committed(&test, &mut shard, a, &[30, 40]).await;
                    let frames = taken(&mut shard, second.key.into(), 0);
                    assert_eq!(frames, [seq(2, 1), seq(3, 1)]);
                });
            }

            #[test]
            fn opens_below_released_and_reads_every_entry_then_live() {
                run(124, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20, 30, 40]).await;
                    assert_eq!(taken(&mut shard, first.key.into(), 0).len(), 4);
                    shard.ack(first.key, live(2)).expect("forward");
                    close(&mut shard, first.key.into());
                    let second = named(&mut shard, "a", "r");
                    assert_eq!(woken(&mut shard), []);
                    let woken = committed(&test, &mut shard, a, &[50]).await;
                    assert_eq!(woken, []);
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(2, 1), seq(3, 1), seq(4, 1)]);
                    let woken = committed(&test, &mut shard, a, &[60]).await;
                    assert_eq!(woken, [second.key.into()]);
                    assert_eq!(taken(&mut shard, second.key.into(), 0), [seq(5, 1)]);
                });
            }

            /// As `opens_below_released_and_reads_every_entry_then_live`, with a shed
            /// and a carry inside the hold.
            #[test]
            fn holds_no_position_after_a_shed_and_a_carry() {
                run(143, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20, 30, 40]).await;
                    assert_eq!(taken(&mut shard, first.key.into(), 0).len(), 4);
                    shard.ack(first.key, live(2)).expect("forward");
                    close(&mut shard, first.key.into());
                    shard.close_writer(a);
                    shard.shed(Slot::new(0));
                    assert!(shard.carry(Slot::new(0), common::key(Slot::new(0))));
                    let second = named(&mut shard, "a", "r");
                    let b = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    committed(&test, &mut shard, b, &[50]).await;
                    assert_eq!(taken(&mut shard, second.key.into(), 0), [seq(4, 1)]);
                });
            }

            #[test]
            fn starts_at_the_live_tail_after_its_hold_ends() {
                run(125, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20]).await;
                    shard.ack(first.key, live(1)).expect("forward");
                    close(&mut shard, first.key.into());
                    test.clock.sleep(HOLD).await;
                    let second = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[30]).await;
                    assert_eq!(taken(&mut shard, second.key.into(), 0), [seq(2, 1)]);
                });
            }

            #[test]
            fn gives_its_held_position_as_its_start_then_the_live_tail_after_the_hold()
            {
                run(248, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20, 30]).await;
                    shard.ack(first.key, live(1)).expect("forward");
                    let open = |shard: &mut Shard| {
                        let opened = shard.open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            HOLD,
                            CREDIT,
                            Charge::Whole,
                        );
                        opened.expect("synced")
                    };
                    let held = open(&mut shard);
                    assert_eq!(held.live, 1);
                    close(&mut shard, held.key.into());
                    test.clock.sleep(HOLD).await;
                    assert_eq!(open(&mut shard).live, 3);
                });
            }

            #[test]
            fn holds_the_position_of_a_complete_session_that_a_latest_open_took_over() {
                run(126, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20]).await;
                    assert_eq!(taken(&mut shard, first.key.into(), 0).len(), 2);
                    shard.ack(first.key, live(1)).expect("forward");
                    let latest = shard.open_named_latest(Slot::new(0), key("a", "r"));
                    let latest = latest.expect("synced");
                    assert_eq!(latest.live, 1);
                    assert_eq!(latest.replaced, Some(first.key.into()));
                    assert_eq!(taken(&mut shard, first.key.into(), 0), []);
                    assert_eq!(taken(&mut shard, latest.key, 0), [seq(1, 1)]);
                    write(&test, &mut shard, a, &[30]);
                    let second = named(&mut shard, "a", "r");
                    assert_eq!(second.replaced, Some(latest.key));
                    assert_eq!(taken(&mut shard, latest.key, 0), []);
                    // At the live tail it would take the next frame.
                    let woken = committed(&test, &mut shard, a, &[40]).await;
                    assert_eq!(woken, []);
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1), seq(2, 1), seq(3, 1)]);
                });
            }

            #[test]
            fn takes_over_a_latest_session_with_a_latest_open() {
                run(127, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = latest(&mut shard, "a", "r");
                    assert_eq!(first.live, 0);
                    write(&test, &mut shard, a, &[10]);
                    let second = latest(&mut shard, "a", "r");
                    assert_eq!((second.live, second.replaced), (0, Some(first.key)));
                    assert_eq!(woken(&mut shard), []);
                    assert_eq!(taken(&mut shard, second.key, 0), [seq(0, 1)]);
                });
            }

            #[test]
            fn holds_until_the_end_of_its_hold_after_a_close() {
                run(128, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20]).await;
                    shard.ack(first.key, live(1)).expect("forward");
                    close(&mut shard, first.key.into());
                    test.clock.sleep(INSIDE).await;
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1)]);
                });
            }

            #[test]
            fn holds_until_the_end_of_its_hold_after_a_latest_open_took_over() {
                run(129, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    committed(&test, &mut shard, a, &[10, 20]).await;
                    shard.ack(first.key, live(1)).expect("forward");
                    let latest = latest(&mut shard, "a", "r");
                    close(&mut shard, latest.key);
                    test.clock.sleep(INSIDE).await;
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1)]);
                });
            }

            #[test]
            fn resumes_from_disk_at_each_open_within_its_hold() {
                run(130, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let mut last = named(&mut shard, "a", "r").key;
                    committed(&test, &mut shard, a, &[10, 20]).await;
                    shard.ack(last, live(1)).expect("forward");
                    for _ in 0..3 {
                        close(&mut shard, last.into());
                        test.clock.sleep(INSIDE).await;
                        last = named(&mut shard, "a", "r").key;
                    }
                    let woken = committed(&test, &mut shard, a, &[30]).await;
                    assert_eq!(woken, []);
                    let frames = caught_up(&mut shard, last, 0).await;
                    assert_eq!(frames, [seq(1, 1), seq(2, 1)]);
                });
            }

            /// Writes a frame at seq 0 that the named reader `r` of `a` takes and acks,
            /// then closes it, so that it holds live seq 1.
            async fn held(test: &Test, shard: &mut Shard, a: writer::Key) {
                let first = named(shard, "a", "r");
                committed(test, shard, a, &[10]).await;
                assert_eq!(taken(shard, first.key.into(), 0), [seq(0, 1)]);
                shard.ack(first.key, live(1)).expect("forward");
                close(shard, first.key.into());
            }

            /// Writes a live frame of one sample at `stamp` that the full pool loses.
            fn lose(test: &Test, shard: &mut Shard, a: writer::Key, stamp: i64) {
                let write =
                    frame(&test.pool, &two_indexes(), &[(0, &[stamp]), (1, &[1])]);
                let blocks = test.fill();
                let written = shard.write(a, LIVE, write).expect("written");
                assert!(matches!(written, [Outcome::Lost { .. }]), "{written:?}");
                drop(blocks);
            }

            #[test]
            fn reads_nothing_from_disk_until_a_grant() {
                run(131, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    committed(&test, &mut shard, a, &[20]).await;
                    let second = shard
                        .open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            HOLD,
                            0,
                            Charge::Whole,
                        )
                        .expect("synced");
                    shard.committed().await.expect("the commit ends");
                    let (frames, end, reads) = read(&mut shard, second.key).await;
                    assert!(frames.is_empty() && reads == 0, "{reads} reads");
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                    shard.grant(second.key, CREDIT);
                    assert_eq!(woken(&mut shard), []);
                    assert_eq!(caught_up(&mut shard, second.key, 0).await, [seq(1, 1)]);
                });
            }

            #[test]
            fn reads_nothing_until_the_commit_of_frames_written_before_its_open() {
                run(132, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    write(&test, &mut shard, a, &[20]);
                    let second = named(&mut shard, "a", "r");
                    let (frames, end, reads) = read(&mut shard, second.key).await;
                    assert!(frames.is_empty() && reads == 0, "{reads} reads");
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                    let (frames, end, reads) = read(&mut shard, second.key).await;
                    assert!(frames.is_empty() && reads == 0, "{reads} reads");
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                    shard.committed().await.expect("the commit ends");
                    assert_eq!(woken(&mut shard), [second.key.into()]);
                    let (frames, end, reads) = read(&mut shard, second.key).await;
                    assert_eq!((ranges(&frames, 0), reads), (vec![seq(1, 1)], 1));
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                    let woken = committed(&test, &mut shard, a, &[30]).await;
                    assert_eq!(woken, [second.key.into()]);
                    assert_eq!(taken(&mut shard, second.key.into(), 0), [seq(2, 1)]);
                });
            }

            #[test]
            fn reads_past_a_frame_lost_before_a_later_append() {
                run(133, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    write(&test, &mut shard, a, &[20]);
                    lose(&test, &mut shard, a, 30);
                    committed(&test, &mut shard, a, &[40]).await;
                    assert_eq!(stored(&shard, Slot::new(0), Path::Live), 4);
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1), seq(3, 1)]);
                });
            }

            #[test]
            fn reads_past_lost_seqs_on_each_side_of_a_handoff() {
                run(144, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    lose(&test, &mut shard, a, 30);
                    shard.close_writer(a);
                    let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
                    lose(&test, &mut shard, b, 40);
                    committed(&test, &mut shard, b, &[50]).await;
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let (frames, end, reads) = read(&mut shard, second.key).await;
                    assert_eq!((ranges(&frames, 0), reads), (vec![seq(3, 1)], 3));
                    assert_eq!(frames[0].series(0), Some(&encoded(&[50])[..]));
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                });
            }

            #[test]
            fn reads_past_a_named_entry_between_two_lost_frames() {
                run(145, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    write(&test, &mut shard, a, &[20]);
                    lose(&test, &mut shard, a, 30);
                    let other = named(&mut shard, "a", "s");
                    lose(&test, &mut shard, a, 50);
                    committed(&test, &mut shard, a, &[60]).await;
                    assert_eq!(stored(&shard, Slot::new(0), Path::Live), 5);
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1), seq(4, 1)]);
                    close(&mut shard, other.key.into());
                });
            }

            #[test]
            fn reads_past_a_handoff_with_little_credit_once_it_acked_each_frame() {
                run(146, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    for stamp in [20, 21, 22, 23] {
                        write(&test, &mut shard, a, &[stamp]);
                    }
                    lose(&test, &mut shard, a, 30);
                    shard.close_writer(a);
                    let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
                    lose(&test, &mut shard, b, 40);
                    committed(&test, &mut shard, b, &[50]).await;
                    let second = shard
                        .open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            HOLD,
                            1,
                            Charge::Whole,
                        )
                        .expect("synced");
                    shard.committed().await.expect("the commit ends");
                    let (frames, end, _) = read(&mut shard, second.key).await;
                    assert_eq!(ranges(&frames, 0), [seq(1, 1)]);
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                    let charge = frames[0].charge();
                    shard.ack(second.key, live(2)).expect("forward");
                    shard.grant(second.key, 5 * charge);
                    let (frames, end, _) = read(&mut shard, second.key).await;
                    assert_eq!(ranges(&frames, 0), [seq(2, 1), seq(3, 1), seq(4, 1)]);
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                    shard.ack(second.key, live(5)).expect("forward");
                    let (frames, end, reads) = read(&mut shard, second.key).await;
                    assert_eq!(
                        ranges(&frames, 0),
                        [seq(7, 1)],
                        "{end:?} {reads} reads"
                    );
                });
            }

            #[test]
            fn reads_up_to_frames_lost_at_the_tail() {
                run(134, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    write(&test, &mut shard, a, &[20]);
                    lose(&test, &mut shard, a, 30);
                    shard.committed().await.expect("the commit ends");
                    assert_eq!(stored(&shard, Slot::new(0), Path::Live), 2);
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1)]);
                });
            }

            #[test]
            fn reads_past_each_lost_run_after_the_commit() {
                run(135, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    write(&test, &mut shard, a, &[20]);
                    let second = named(&mut shard, "a", "r");
                    let (frames, _, reads) = read(&mut shard, second.key).await;
                    assert!(frames.is_empty() && reads == 0, "{reads} reads");
                    for stamp in [30, 50] {
                        lose(&test, &mut shard, a, stamp);
                        write(&test, &mut shard, a, &[stamp + 10]);
                    }
                    shard.committed().await.expect("the commit ends");
                    assert_eq!(woken(&mut shard), [second.key.into()]);
                    let frames = caught_up(&mut shard, second.key, 0).await;
                    assert_eq!(frames, [seq(1, 1), seq(3, 1), seq(5, 1)]);
                });
            }

            #[test]
            fn drops_a_read_after_a_takeover() {
                run(136, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    committed(&test, &mut shard, a, &[20, 30]).await;
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let reader::Next::Disk(disk) = shard.take(second.key.into()) else {
                        panic!("the reader reads from disk");
                    };
                    let third = named(&mut shard, "a", "r");
                    assert_eq!(third.replaced, Some(second.key.into()));
                    let read = disk.read().await.expect("the read ends");
                    let set = disk_set(Slot::new(0));
                    assert_eq!(shard.fill(read, &set), Ok(()));
                    assert_eq!(taken(&mut shard, second.key.into(), 0), []);
                    let frames = caught_up(&mut shard, third.key, 0).await;
                    assert_eq!(frames, [seq(1, 1), seq(2, 1)]);
                });
            }

            #[test]
            fn reads_past_more_handoffs_at_one_seq_than_a_read_takes() {
                run(137, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    held(&test, &mut shard, a).await;
                    shard.close_writer(a);
                    for _ in 0..3 {
                        let b =
                            shard.open_writer(writer("b", 1, &set)).expect("synced");
                        shard.close_writer(b);
                    }
                    let c = shard.open_writer(writer("c", 1, &set)).expect("synced");
                    committed(&test, &mut shard, c, &[20]).await;
                    let second = shard
                        .open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            HOLD,
                            1,
                            Charge::Whole,
                        )
                        .expect("synced");
                    shard.committed().await.expect("the commit ends");
                    let set = disk_set(Slot::new(0));
                    let (mut frames, mut reads) = (Vec::new(), 0);
                    let end = loop {
                        let (taken, end) = drain(&mut shard, second.key);
                        frames.extend(taken);
                        let reader::Next::Disk(disk) = end else {
                            break end;
                        };
                        reads += 1;
                        // Each read takes one entry, so it stops within a seq.
                        let read =
                            disk.with_budget(1).read().await.expect("the read ends");
                        shard.fill(read, &set).expect("the pool has a block");
                    };
                    // The named entry of the open adds one read.
                    assert_eq!((ranges(&frames, 0), reads), (vec![seq(1, 1)], 10));
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                });
            }

            #[test]
            fn reads_again_when_a_read_stops_at_its_budget_below_the_durable_end() {
                run(138, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(1 << 21).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = named(&mut shard, "a", "r");
                    close(&mut shard, first.key.into());
                    // Values that the codec does not shrink: about 100 stored frames
                    // fill the budget of a read.
                    for n in 0..150 {
                        let stamps: Vec<i64> = (n * 300 + 1..=(n + 1) * 300).collect();
                        let values: Vec<_> = stamps
                            .iter()
                            .map(|&stamp| {
                                let mixed = stamp.wrapping_mul(0x1e37_79b9_7f4a_7c15);
                                (mixed ^ (mixed >> 29))
                                    .wrapping_mul(0x3f58_476d_1ce4_e5b9)
                            })
                            .collect();
                        let write =
                            frame(&test.pool, &set, &[(0, &stamps), (1, &values)]);
                        let written = shard.write(a, LIVE, write).expect("written");
                        assert!(matches!(written, [Outcome::Applied { .. }]));
                        if n % 10 == 9 {
                            shard.committed().await.expect("the commit ends");
                        }
                    }
                    let second = named(&mut shard, "a", "r");
                    shard.committed().await.expect("the commit ends");
                    let (frames, end, reads) = read(&mut shard, second.key).await;
                    let all: Vec<_> = (0..150).map(|n| seq(n * 300, 300)).collect();
                    assert_eq!((ranges(&frames, 0), reads), (all, 2));
                    assert!(matches!(end, reader::Next::Empty), "{end:?}");
                });
            }

            mod records {
                use super::*;

                /// A hold that no test reaches.
                const LONG: Span = Span::from_nanos(3_600_000_000_000);

                /// The key of the reader of the longest subject and name, with a
                /// subject that starts with `n`.
                fn longest(n: usize) -> named::Key {
                    let n = n.to_string();
                    let subject =
                        format!("{n}{}", "s".repeat(Name::MAX_BYTES - n.len()));
                    key(&subject, &"r".repeat(Name::MAX_BYTES))
                }

                /// Opens `reader` on the index at slot 0 with a hold of `LONG`.
                fn open_longest(
                    shard: &mut Shard,
                    reader: named::Key,
                ) -> Result<reader::Opened<complete::Key>, Unopened> {
                    shard.open_named_complete(
                        Slot::new(0),
                        reader,
                        LONG,
                        CREDIT,
                        Charge::Whole,
                    )
                }

                /// Opens the named complete reader `r` of `subject` on the index at
                /// slot 0, with a hold of `LONG`.
                fn open(shard: &mut Shard, subject: &str) -> complete::Key {
                    shard
                        .open_named_complete(
                            Slot::new(0),
                            key(subject, "r"),
                            LONG,
                            CREDIT,
                            Charge::Whole,
                        )
                        .expect("opens")
                        .key
                }

                /// Runs `before` on a node with a writer `w` on its shard, waits for
                /// the commit, and cuts the power. Then runs `after` on a new shard
                /// of the node.
                fn restart<B, A>(
                    seed: u64,
                    before: impl AsyncFnOnce(&Test, &mut Shard, writer::Key) -> B
                    + Send
                    + 'static,
                    after: impl AsyncFnOnce(&Test, &mut Shard) -> A + Send + 'static,
                ) {
                    let (mut sim, node) = create_node(seed);
                    sim.run_on(&node, |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        before(&test, &mut shard, w).await;
                        shard.committed().await.expect("the commit ends");
                    })
                    .expect("the first run ends");
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let mut shard = test.shard(AREA).await;
                        after(&test, &mut shard).await;
                    })
                    .expect("the run after the cut ends");
                }

                /// Writes frames at seq 0 and 1, and acks seq 0 to the reader of `a`.
                async fn acked(test: &Test, shard: &mut Shard, w: writer::Key) {
                    let a = open(shard, "a");
                    committed(test, shard, w, &[10, 20]).await;
                    assert_eq!(taken(shard, a.into(), 0), [seq(0, 1), seq(1, 1)]);
                    shard.ack(a, live(1)).expect("forward");
                }

                /// Fills the pool, then frees the smallest block that holds the body of
                /// a part, so that one part can append. Returns the blocks it keeps.
                fn fill_but_one(test: &Test) -> Vec<block::Unique> {
                    let mut blocks = test.fill();
                    let part = blocks.iter().enumerate();
                    let part = part.filter(|(_, block)| block.len() >= 4032);
                    let part = part.min_by_key(|(_, block)| block.len());
                    drop(blocks.swap_remove(part.expect("a block").0));
                    blocks
                }

                /// Sleeps one interval and advances the shard.
                async fn tick(test: &Test, shard: &mut Shard) {
                    test.clock.sleep(INTERVAL).await;
                    shard.advance();
                }

                #[test]
                fn restores_each_named_position_after_a_restart() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let b = open(shard, "b");
                        acked(test, shard, w).await;
                        shard.close_reader(b.into());
                        tick(test, shard).await;
                    };
                    restart(190, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, a, 0).await, [seq(1, 1)]);
                        let b = open(shard, "b");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(
                            caught_up(shard, b, 0).await,
                            [seq(0, 1), seq(1, 1)]
                        );
                        let c = open(shard, "c");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, c, 0).await, []);
                    });
                }

                #[test]
                fn restores_each_position_under_its_subject() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        open(shard, "b");
                        acked(test, shard, w).await;
                    };
                    restart(191, before, async |_, shard: &mut Shard| {
                        let b = open(shard, "b");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(
                            caught_up(shard, b, 0).await,
                            [seq(0, 1), seq(1, 1)]
                        );
                    });
                }

                #[test]
                fn writes_an_acked_position_only_at_the_tick() {
                    restart(192, acked, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(
                            caught_up(shard, a, 0).await,
                            [seq(0, 1), seq(1, 1)]
                        );
                    });
                }

                #[test]
                fn writes_an_acked_position_at_the_tick() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        acked(test, shard, w).await;
                        let deadline = shard.deadline().expect("a tick");
                        let now = test.reader.now().monotonic;
                        assert!(now < deadline && deadline <= now + INTERVAL);
                        tick(test, shard).await;
                    };
                    restart(193, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, a, 0).await, [seq(1, 1)]);
                    });
                }

                #[test]
                fn writes_the_position_at_a_takeover() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        acked(test, shard, w).await;
                        open(shard, "a");
                    };
                    restart(194, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, a, 0).await, [seq(1, 1)]);
                    });
                }

                #[test]
                fn ends_the_holds_in_the_buffer_at_a_shed() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        acked(test, shard, w).await;
                        let a = open(shard, "a");
                        shard.close_reader(a.into());
                        shard.close_writer(w);
                        shard.shed(Slot::new(0));
                    };
                    restart(195, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, a, 0).await, []);
                    });
                }

                #[test]
                fn restores_after_mesh_time_when_carried_before_it() {
                    let (mut sim, node) = create_node(196);
                    sim.run_on(&node, |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        acked(&test, &mut shard, w).await;
                        tick(&test, &mut shard).await;
                        shard.committed().await.expect("the commit ends");
                    })
                    .expect("the first run ends");
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
                        let (clock, reader) = clock::Clock::new(test.clock.clone());
                        let mut shard = Test::with(0, buffer, reader).await;
                        assert!(shard.carry(Slot::new(0), common::key(Slot::new(0))));
                        assert!(shard.deadline().is_some(), "the records wait");
                        let wall = test.node.wall();
                        test.tasks.spawn(async move { clock.run(wall).await });
                        while shard.now().is_none() {
                            test.clock.sleep(Span::from_nanos(1)).await;
                        }
                        let a = open(&mut shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(1, 1)]);
                    })
                    .expect("the run after the cut ends");
                }

                #[test]
                fn ticks_each_second_while_another_index_holds() {
                    let (mut sim, node) = create_node(232);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let mut shard = test.shard(AREA).await;
                        let _a = open(&mut shard, "a");
                        let h = shard
                            .open_named_complete(
                                Slot::new(2),
                                key("h", "r"),
                                LONG,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("opens")
                            .key;
                        shard.close_reader(h.into());
                        tick(&test, &mut shard).await;
                        let deadline = shard.deadline().expect("a tick");
                        let now = test.reader.now().monotonic;
                        assert!(deadline <= now + INTERVAL, "a is open");
                    })
                    .expect("the run ends");
                }

                #[test]
                fn resumes_a_takeover_with_a_new_hold_whose_entry_found_no_block() {
                    let hold = Span::from_nanos(2_000_000_000);
                    let open_with = move |shard: &mut Shard, hold: Span| {
                        shard.open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            hold,
                            CREDIT,
                            Charge::Whole,
                        )
                    };
                    let (mut sim, node) = create_node(231);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let a = open_with(&mut shard, Span::ZERO).expect("opens").key;
                        committed(&test, &mut shard, w, &[10]).await;
                        assert_eq!(taken(&mut shard, a.into(), 0), [seq(0, 1)]);
                        shard.committed().await.expect("the commit ends");
                        let blocks = test.fill();
                        let refused = open_with(&mut shard, hold);
                        assert_eq!(refused, Err(Unopened::Unrecorded));
                        drop(blocks);
                        let _a = open_with(&mut shard, hold).expect("opens");
                        shard.committed().await.expect("the commit ends");
                    })
                    .expect("the first run ends");
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        test.clock.sleep(Span::from_nanos(500_000_000)).await;
                        let mut shard = test.shard(AREA).await;
                        let a = open_with(&mut shard, hold).expect("opens").key;
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(0, 1)]);
                    })
                    .expect("the run after the cut ends");
                }

                #[test]
                fn resumes_a_reopen_in_its_hold_whose_entry_found_no_block() {
                    let hold = Span::from_nanos(2_000_000_000);
                    let open_held = move |shard: &mut Shard| {
                        shard
                            .open_named_complete(
                                Slot::new(0),
                                key("a", "r"),
                                hold,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("opens")
                            .key
                    };
                    let (mut sim, node) = create_node(230);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let a = open_held(&mut shard);
                        committed(&test, &mut shard, w, &[10]).await;
                        assert_eq!(taken(&mut shard, a.into(), 0), [seq(0, 1)]);
                        shard.ack(a, live(1)).expect("forward");
                        shard.close_reader(a.into());
                        shard.committed().await.expect("the commit ends");
                        test.clock.sleep(Span::from_nanos(1_500_000_000)).await;
                        let blocks = test.fill();
                        let refused = shard.open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            hold,
                            CREDIT,
                            Charge::Whole,
                        );
                        assert_eq!(refused, Err(Unopened::Unrecorded));
                        drop(blocks);
                        let a = open_held(&mut shard);
                        committed(&test, &mut shard, w, &[20]).await;
                        assert_eq!(taken(&mut shard, a.into(), 0), [seq(1, 1)]);
                        shard.committed().await.expect("the commit ends");
                    })
                    .expect("the first run ends");
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        test.clock.sleep(Span::from_nanos(3_000_000_000)).await;
                        let mut shard = test.shard(AREA).await;
                        let a = open_held(&mut shard);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(1, 1)]);
                    })
                    .expect("the run after the cut ends");
                }

                #[test]
                fn reads_a_reopen_from_disk_after_its_entry_commits() {
                    let hold = Span::from_nanos(2_000_000_000);
                    let open_held = move |shard: &mut Shard| {
                        shard
                            .open_named_complete(
                                Slot::new(0),
                                key("a", "r"),
                                hold,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("opens")
                            .key
                    };
                    let (mut sim, node) = create_node(700);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let a = open_held(&mut shard);
                        committed(&test, &mut shard, w, &[10, 20]).await;
                        assert_eq!(
                            taken(&mut shard, a.into(), 0),
                            [seq(0, 1), seq(1, 1)]
                        );
                        shard.ack(a, live(1)).expect("forward");
                        shard.close_reader(a.into());
                        shard.committed().await.expect("the commit ends");
                        test.clock.sleep(Span::from_nanos(1_500_000_000)).await;
                        let a = open_held(&mut shard);
                        assert_eq!(caught_up(&mut shard, a, 0).await, []);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(woken(&mut shard), [a.into()]);
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(1, 1)]);
                    })
                    .expect("the first run ends");
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        test.clock.sleep(Span::from_nanos(3_000_000_000)).await;
                        let mut shard = test.shard(AREA).await;
                        let a = open_held(&mut shard);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(1, 1)]);
                    })
                    .expect("the run after the cut ends");
                }

                #[test]
                fn reads_a_reopen_from_disk_after_its_entry_commits_while_a_sync_runs()
                {
                    run(701, |test| async move {
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let a = open(&mut shard, "a");
                        committed(&test, &mut shard, w, &[10, 20]).await;
                        assert_eq!(
                            taken(&mut shard, a.into(), 0),
                            [seq(0, 1), seq(1, 1)]
                        );
                        shard.ack(a, live(1)).expect("forward");
                        shard.close_reader(a.into());
                        shard.committed().await.expect("the commit ends");
                        test.clock.sleep(COMMIT).await;
                        write(&test, &mut shard, w, &[30]);
                        let first = shard.committed();
                        test.clock.sleep(SYNC).await;
                        let a = open(&mut shard, "a");
                        assert_eq!(
                            shard.buffer.durable_at(),
                            shard.buffer.commits() + 2,
                            "a sync runs, and the entry waits for the next commit"
                        );
                        first.await.expect("the first commit ends");
                        woken(&mut shard);
                        assert_eq!(caught_up(&mut shard, a, 0).await, []);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(woken(&mut shard), [a.into()]);
                        assert_eq!(
                            caught_up(&mut shard, a, 0).await,
                            [seq(1, 1), seq(2, 1)]
                        );
                    });
                }

                /// Opens the named complete reader `r` of `a` on the index at slot 0,
                /// with a hold of 2 s.
                fn open_held(shard: &mut Shard) -> complete::Key {
                    shard
                        .open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            Span::from_nanos(2_000_000_000),
                            CREDIT,
                            Charge::Whole,
                        )
                        .expect("opens")
                        .key
                }

                #[test]
                fn takes_no_frame_from_memory_before_the_entry_of_a_reopen_commits() {
                    let (mut sim, node) = create_node(702);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let _b = complete(&mut shard, Slot::new(0));
                        let a = open_held(&mut shard);
                        committed(&test, &mut shard, w, &[10, 20]).await;
                        assert_eq!(
                            taken(&mut shard, a.into(), 0),
                            [seq(0, 1), seq(1, 1)]
                        );
                        shard.ack(a, live(2)).expect("forward");
                        shard.close_reader(a.into());
                        shard.committed().await.expect("the commit ends");
                        test.clock.sleep(Span::from_nanos(1_500_000_000)).await;
                        write(&test, &mut shard, w, &[30]);
                        let first = shard.committed();
                        test.clock.sleep(SYNC).await;
                        let a = open_held(&mut shard);
                        first.await.expect("the first commit ends");
                        woken(&mut shard);
                        assert_eq!(taken(&mut shard, a.into(), 0), []);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(woken(&mut shard), [a.into()]);
                        assert_eq!(taken(&mut shard, a.into(), 0), [seq(2, 1)]);
                    })
                    .expect("the first run ends");
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        test.clock.sleep(Span::from_nanos(3_000_000_000)).await;
                        let mut shard = test.shard(AREA).await;
                        let a = open_held(&mut shard);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(2, 1)]);
                    })
                    .expect("the run after the cut ends");
                }

                #[test]
                fn waits_for_the_entry_of_the_session_that_a_takeover_replaces() {
                    let (mut sim, node) = create_node(703);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let a = open_held(&mut shard);
                        committed(&test, &mut shard, w, &[10, 20]).await;
                        assert_eq!(
                            taken(&mut shard, a.into(), 0),
                            [seq(0, 1), seq(1, 1)]
                        );
                        shard.ack(a, live(1)).expect("forward");
                        shard.close_reader(a.into());
                        shard.committed().await.expect("the commit ends");
                        test.clock.sleep(Span::from_nanos(1_500_000_000)).await;
                        let _reopen = open_held(&mut shard);
                        let a = open_held(&mut shard);
                        assert_eq!(caught_up(&mut shard, a, 0).await, []);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(woken(&mut shard), [a.into()]);
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(1, 1)]);
                    })
                    .expect("the first run ends");
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, move |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        test.clock.sleep(Span::from_nanos(3_000_000_000)).await;
                        let mut shard = test.shard(AREA).await;
                        let a = open_held(&mut shard);
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(&mut shard, a, 0).await, [seq(1, 1)]);
                    })
                    .expect("the run after the cut ends");
                }

                #[test]
                fn wakes_no_reader_that_closed_while_its_entry_waits() {
                    run(704, |test| async move {
                        let set = two_indexes();
                        let mut shard = test.shard(AREA).await;
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let a = open_held(&mut shard);
                        committed(&test, &mut shard, w, &[10, 20]).await;
                        assert_eq!(
                            taken(&mut shard, a.into(), 0),
                            [seq(0, 1), seq(1, 1)]
                        );
                        shard.ack(a, live(1)).expect("forward");
                        shard.close_reader(a.into());
                        shard.committed().await.expect("the commit ends");
                        let a = open_held(&mut shard);
                        assert_eq!(caught_up(&mut shard, a, 0).await, []);
                        shard.close_reader(a.into());
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(woken(&mut shard), []);
                    });
                }

                #[test]
                fn keeps_the_old_position_when_the_tick_finds_no_block() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let a = open(shard, "a");
                        committed(test, shard, w, &[10, 20]).await;
                        assert_eq!(taken(shard, a.into(), 0), [seq(0, 1), seq(1, 1)]);
                        let blocks = test.fill();
                        shard.ack(a, live(1)).expect("forward");
                        tick(test, shard).await;
                        shard.committed().await.expect("the commit ends");
                        drop(blocks);
                    };
                    restart(202, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(
                            caught_up(shard, a, 0).await,
                            [seq(0, 1), seq(1, 1)]
                        );
                    });
                }

                #[test]
                fn appends_records_that_found_no_block_at_the_next_tick() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let a = open(shard, "a");
                        committed(test, shard, w, &[10, 20]).await;
                        assert_eq!(taken(shard, a.into(), 0), [seq(0, 1), seq(1, 1)]);
                        let blocks = test.fill();
                        shard.ack(a, live(1)).expect("forward");
                        tick(test, shard).await;
                        drop(blocks);
                        tick(test, shard).await;
                    };
                    restart(197, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, a, 0).await, [seq(1, 1)]);
                    });
                }

                #[test]
                fn refuses_a_named_open_when_no_part_has_room() {
                    run(198, |test| async move {
                        // A ring with room for the 448 entries that the opens append.
                        let mut shard = test.shard(1 << 22).await;
                        let mut opened = Vec::new();
                        for n in 0..448 {
                            let reader = open_longest(&mut shard, longest(n));
                            opened.push(reader.expect("a part has room").key);
                            shard.committed().await.expect("the commit ends");
                        }
                        let full = open_longest(&mut shard, longest(448));
                        assert_eq!(full, Err(Unopened::Full { limit: 257_728 }));
                        let again = open_longest(&mut shard, longest(0));
                        let again = again.expect("its own record is replaced");
                        assert_eq!(again.replaced, Some(opened[0].into()));
                        close(&mut shard, opened[1].into());
                        let reopened = open_longest(&mut shard, longest(1));
                        reopened.expect("its record keeps its part");
                    });
                }

                #[test]
                fn frees_the_room_of_a_key_whose_reopen_found_no_block() {
                    run(298, |test| async move {
                        let mut shard = test.shard(1 << 22).await;
                        let k = shard
                            .open_named_complete(
                                Slot::new(0),
                                longest(0),
                                Span::ZERO,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("opens")
                            .key;
                        shard.committed().await.expect("the commit ends");
                        for n in 1..448 {
                            let reader = open_longest(&mut shard, longest(n));
                            reader.expect("a part has room");
                            shard.committed().await.expect("the commit ends");
                        }
                        close(&mut shard, k.into());
                        tick(&test, &mut shard).await;
                        shard.committed().await.expect("the commit ends");
                        let blocks = test.fill();
                        let refused = open_longest(&mut shard, longest(0));
                        assert_eq!(refused, Err(Unopened::Unrecorded));
                        drop(blocks);
                        for _ in 0..4 {
                            tick(&test, &mut shard).await;
                            shard.committed().await.expect("the commit ends");
                        }
                        // 447 records: the room of longest(0) in part 0 is free.
                        let new = open_longest(&mut shard, longest(448));
                        new.expect("part 0 has the room of longest(0)");
                    });
                }

                #[test]
                fn frees_the_room_of_a_closed_key_before_its_removal_commits() {
                    run(299, |test| async move {
                        let mut shard = test.shard(1 << 22).await;
                        let k = open_k(&mut shard);
                        shard.committed().await.expect("the commit ends");
                        for n in 1..448 {
                            let reader = open_longest(&mut shard, longest(n));
                            reader.expect("a part has room");
                            shard.committed().await.expect("the commit ends");
                        }
                        close(&mut shard, k.into());
                        let new = open_longest(&mut shard, longest(448));
                        new.expect("the close appended the removal of longest(0)");
                    });
                }

                #[test]
                fn restores_each_position_of_more_records_than_one_entry_holds() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let opened: Vec<_> = (0..20)
                            .map(|n| {
                                open_longest(shard, longest(n)).expect("opens").key
                            })
                            .collect();
                        committed(test, shard, w, &[10, 20]).await;
                        for (n, &reader) in opened.iter().enumerate() {
                            let read = taken(shard, reader.into(), 0);
                            assert_eq!(read, [seq(0, 1), seq(1, 1)]);
                            if n % 2 == 1 {
                                shard.ack(reader, live(1)).expect("forward");
                            }
                        }
                        tick(test, shard).await;
                    };
                    restart(203, before, async |_, shard: &mut Shard| {
                        for n in 0..20 {
                            let reader = open_longest(shard, longest(n));
                            let reader = reader.expect("opens").key;
                            shard.committed().await.expect("the commit ends");
                            let read = caught_up(shard, reader, 0).await;
                            let expected = if n % 2 == 1 {
                                &[seq(1, 1)][..]
                            } else {
                                &[seq(0, 1), seq(1, 1)]
                            };
                            assert_eq!(read, expected, "reader {n}");
                        }
                    });
                }

                /// Opens `longest(0)` with a hold of zero.
                fn open_k(shard: &mut Shard) -> complete::Key {
                    let opened = shard.open_named_complete(
                        Slot::new(0),
                        longest(0),
                        Span::ZERO,
                        CREDIT,
                        Charge::Whole,
                    );
                    opened.expect("opens").key
                }

                /// The count of the records of `reader` in the newest named entries
                /// of the index at slot 0 in the buffer of `shard`.
                async fn records_of(shard: &Shard, reader: &named::Key) -> usize {
                    let newest = crate::named::Parts::newest(&shard.buffer).await;
                    let newest = newest.expect("the newest entries");
                    let records = newest.get(&Slot::new(0)).map(|(_, records)| records);
                    let records = records.into_iter().flatten();
                    records.filter(|record| record.reader == *reader).count()
                }

                #[test]
                fn reopens_in_its_part_a_key_whose_removal_found_no_block() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let k = open_k(shard);
                        for n in 1..7 {
                            open_longest(shard, longest(n)).expect("opens");
                        }
                        committed(test, shard, w, &[10, 20]).await;
                        let blocks = test.fill();
                        close(shard, k.into());
                        tick(test, shard).await;
                        drop(blocks);
                        open_longest(shard, longest(7)).expect("part 1 has room");
                        open_k(shard);
                    };
                    restart(214, before, async |_, shard: &mut Shard| {
                        assert_eq!(records_of(shard, &longest(0)).await, 1);
                        let k = open_longest(shard, longest(0));
                        let k = k.expect("its record is in one part").key;
                        shard.committed().await.expect("the commit ends");
                        let read = caught_up(shard, k, 0).await;
                        assert_eq!(read, [], "it resumes where its reopen started");
                    });
                }

                #[test]
                fn restores_once_a_key_that_returns_in_another_part() {
                    for steps in 1..=4 {
                        let before = async move |test: &Test, shard: &mut Shard, w| {
                            let k = open_k(shard);
                            for n in 1..7 {
                                open_longest(shard, longest(n)).expect("opens");
                            }
                            committed(test, shard, w, &[10, 20]).await;
                            if steps > 1 {
                                close(shard, k.into());
                            }
                            if steps > 2 {
                                let new = open_longest(shard, longest(7));
                                new.expect("part 0 has the room of k");
                            }
                            if steps > 3 {
                                open_k(shard);
                            }
                        };
                        restart(215, before, async move |_, shard: &mut Shard| {
                            let records = records_of(shard, &longest(0)).await;
                            let expected = usize::from(steps != 2 && steps != 3);
                            assert_eq!(records, expected, "after step {steps}");
                            let k = open_longest(shard, longest(0));
                            k.expect("its record is in one part at most");
                        });
                    }
                }

                /// The case of the approval of #2833: a close whose removal finds
                /// no block, an add that fills the part, the reopen and a second
                /// close, then a crash.
                #[test]
                fn keeps_a_closed_key_in_its_part_until_its_removal_appends() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let k = open_k(shard);
                        for n in 1..7 {
                            open_longest(shard, longest(n)).expect("opens");
                        }
                        committed(test, shard, w, &[10, 20]).await;
                        let blocks = test.fill();
                        close(shard, k.into());
                        tick(test, shard).await;
                        drop(blocks);
                        open_longest(shard, longest(7)).expect("part 1 has room");
                        let k = open_k(shard);
                        close(shard, k.into());
                        tick(test, shard).await;
                    };
                    restart(207, before, async |_, shard: &mut Shard| {
                        let k = open_longest(shard, longest(0)).expect("opens").key;
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, k, 0).await, [], "k has no record");
                        for n in 1..8 {
                            let reader = open_longest(shard, longest(n));
                            let reader = reader.expect("opens").key;
                            shard.committed().await.expect("the commit ends");
                            let read = caught_up(shard, reader, 0).await;
                            let expected = if n < 7 {
                                &[seq(0, 1), seq(1, 1)][..]
                            } else {
                                &[]
                            };
                            assert_eq!(read, expected, "reader {n}");
                        }
                    });
                }

                /// Opens 8 readers of the longest names, acks seq 0 to the reader in
                /// part 1, and then opens `a`, for which only part 0 finds a block.
                async fn append_part_0_only(
                    test: &Test,
                    shard: &mut Shard,
                    w: writer::Key,
                ) {
                    let opened: Vec<_> = (0..8)
                        .map(|n| open_longest(shard, longest(n)).expect("opens").key)
                        .collect();
                    committed(test, shard, w, &[10, 20]).await;
                    assert_eq!(taken(shard, opened[7].into(), 0).len(), 2);
                    shard.ack(opened[7], live(1)).expect("forward");
                    let blocks = test.fill();
                    tick(test, shard).await;
                    drop(blocks);
                    // The frame gives the open group its blocks, so an entry takes
                    // only the block of its body.
                    write(test, shard, w, &[30]);
                    let blocks = fill_but_one(test);
                    let a = shard.open_named_complete(
                        Slot::new(0),
                        key("a", "r"),
                        LONG,
                        CREDIT,
                        Charge::Whole,
                    );
                    a.expect("part 0 appends its record");
                    drop(blocks);
                }

                #[test]
                fn opens_a_reader_whose_part_appends_before_a_part_that_finds_no_block()
                {
                    restart(208, append_part_0_only, async |_, shard: &mut Shard| {
                        let reader =
                            open_longest(shard, longest(7)).expect("opens").key;
                        shard.committed().await.expect("the commit ends");
                        let read = caught_up(shard, reader, 0).await;
                        assert_eq!(
                            read,
                            [seq(0, 1), seq(1, 1), seq(2, 1)],
                            "the ack found no block"
                        );
                    });
                }

                #[test]
                fn appends_at_the_next_tick_a_part_that_found_no_block() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        append_part_0_only(test, shard, w).await;
                        tick(test, shard).await;
                    };
                    restart(211, before, async |_, shard: &mut Shard| {
                        let reader =
                            open_longest(shard, longest(7)).expect("opens").key;
                        shard.committed().await.expect("the commit ends");
                        let read = caught_up(shard, reader, 0).await;
                        assert_eq!(
                            read,
                            [seq(1, 1), seq(2, 1)],
                            "the tick appends the ack"
                        );
                    });
                }

                #[test]
                fn changes_no_key_of_a_part_that_found_no_block() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let hold_0 = |shard: &mut Shard, reader| {
                            let opened = shard.open_named_complete(
                                Slot::new(0),
                                reader,
                                Span::ZERO,
                                CREDIT,
                                Charge::Whole,
                            );
                            opened.expect("opens").key
                        };
                        for n in 0..6 {
                            open_longest(shard, longest(n)).expect("opens in part 0");
                        }
                        let z = hold_0(shard, key(&"z".repeat(150), &"y".repeat(150)));
                        let x = hold_0(shard, longest(7));
                        open_longest(shard, longest(8)).expect("opens in part 1");
                        committed(test, shard, w, &[10, 20]).await;
                        let blocks = test.fill();
                        close(shard, x.into());
                        close(shard, z.into());
                        tick(test, shard).await;
                        drop(blocks);
                        write(test, shard, w, &[30]);
                        let blocks = fill_but_one(test);
                        let a = shard.open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            LONG,
                            CREDIT,
                            Charge::Whole,
                        );
                        a.expect("part 0 appends its record");
                        shard.committed().await.expect("the commit ends");
                        drop(blocks);
                        write(test, shard, w, &[40]);
                        test.clock.sleep(INTERVAL).await;
                        let blocks = test.fill();
                        shard.advance();
                        shard.committed().await.expect("the commit ends");
                        drop(blocks);
                        write(test, shard, w, &[50]);
                        let blocks = fill_but_one(test);
                        hold_0(shard, longest(7));
                        shard.committed().await.expect("the commit ends");
                        drop(blocks);
                    };
                    restart(212, before, async |_, shard: &mut Shard| {
                        let reader = open_longest(shard, longest(7));
                        reader.expect("its record is in one part");
                    });
                }

                #[test]
                fn frees_the_room_of_a_new_key_whose_part_found_no_block() {
                    run(213, |test| async move {
                        let mut shard = test.shard(1 << 22).await;
                        for n in 0..447 {
                            let reader = open_longest(&mut shard, longest(n));
                            reader.expect("a part has room");
                            shard.committed().await.expect("the commit ends");
                        }
                        let blocks = test.fill();
                        let refused = open_longest(&mut shard, longest(447));
                        assert_eq!(refused, Err(Unopened::Unrecorded));
                        drop(blocks);
                        let new = open_longest(&mut shard, longest(448));
                        new.expect("part 63 has the room of longest(447)");
                    });
                }

                #[test]
                fn keeps_the_parts_of_a_shed_index_whose_entry_found_no_block() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let ticking = shard.open_named_complete(
                            Slot::new(2),
                            key("t", "r"),
                            LONG,
                            CREDIT,
                            Charge::Whole,
                        );
                        ticking.expect("opens");
                        for n in 0..8 {
                            let reader = open_longest(shard, longest(n));
                            close(shard, reader.expect("opens").key.into());
                        }
                        tick(test, shard).await;
                        shard.close_writer(w);
                        let blocks = test.fill();
                        shard.shed(Slot::new(0));
                        drop(blocks);
                        tick(test, shard).await;
                        assert!(shard.carry(Slot::new(0), common::key(Slot::new(0))));
                        open_longest(shard, longest(7)).expect("opens in part 1");
                    };
                    restart(209, before, async |_, shard: &mut Shard| {
                        let reader = open_longest(shard, longest(7));
                        reader.expect("its record is in one part");
                    });
                }

                #[test]
                fn drops_a_record_whose_hold_ends_while_another_index_ticks() {
                    run(210, |test| async move {
                        let mut shard = test.shard(AREA).await;
                        let _ticking = open(&mut shard, "t");
                        let held = shard.open_named_complete(
                            Slot::new(2),
                            key("h", "r"),
                            Span::from_nanos(2_000_000_000),
                            CREDIT,
                            Charge::Whole,
                        );
                        shard.close_reader(held.expect("opens").key.into());
                        for _ in 0..4 {
                            tick(&test, &mut shard).await;
                        }
                        shard.committed().await.expect("the commit ends");
                        let newest = crate::named::Parts::newest(&shard.buffer).await;
                        let newest = newest.expect("the newest entries");
                        assert_eq!(newest[&Slot::new(2)].1, [], "the hold ended");
                    });
                }

                #[test]
                fn sheds_each_part() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let opened: Vec<_> = (0..8)
                            .map(|n| {
                                open_longest(shard, longest(n)).expect("opens").key
                            })
                            .collect();
                        committed(test, shard, w, &[10, 20]).await;
                        for reader in opened {
                            shard.ack(reader, live(1)).expect("forward");
                            close(shard, reader.into());
                        }
                        tick(test, shard).await;
                        shard.close_writer(w);
                        shard.shed(Slot::new(0));
                        // The parts of a shed index show only in memory.
                        assert!(!shard.named.contains_key(&Slot::new(0)));
                    };
                    restart(204, before, async |_, shard: &mut Shard| {
                        for n in [0, 7] {
                            let reader = open_longest(shard, longest(n));
                            let reader = reader.expect("opens").key;
                            shard.committed().await.expect("the commit ends");
                            assert_eq!(caught_up(shard, reader, 0).await, [], "{n}");
                        }
                    });
                }

                /// Opens a shard over a ring that holds a named entry of each tag of
                /// `entries`, with its body, for the index at slot 0.
                fn open_over(
                    seed: u64,
                    entries: &[(u8, &'static [u8])],
                ) -> Result<(), sim::Error> {
                    let entries = entries.to_vec();
                    let (mut sim, node) = create_node(seed);
                    sim.run_on(&node, |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
                        for (tag, body) in entries {
                            let body = test.pool.copy(body).expect("a block");
                            let record = Entry {
                                index: common::key(Slot::new(0)),
                                slot: Slot::new(0),
                                path: Path::Live,
                                first: 0,
                                samples: None,
                                stored_at: Stamp::from_nanos(1),
                                tag,
                                parts: Some(body).into(),
                            };
                            buffer.append([record]).expect("the ring has room");
                        }
                        buffer.committed().await.expect("the commit ends");
                        test.over(buffer).await;
                    })
                }

                #[test]
                fn reads_the_parts_up_to_the_first_tag_with_no_entry() {
                    const OTHER: &[u8] = &[2, 0, 0, 0, 0];
                    open_over(205, &[(3, OTHER)]).expect("it reads no part past 0");
                    let ended = open_over(206, &[(2, &[1, 0, 0, 0, 0]), (3, OTHER)]);
                    let Err(sim::Error::Panicked { message, .. }) = ended else {
                        panic!("the open reads part 1: {ended:?}");
                    };
                    assert_eq!(
                        message,
                        "the named entry has version 2; this build reads version 1"
                    );
                }

                #[test]
                fn arms_the_tick_at_the_end_of_the_hold() {
                    run(199, |test| async move {
                        let mut shard = test.shard(AREA).await;
                        assert_eq!(shard.deadline(), None);
                        let hold = Span::from_nanos(5_500_000_000);
                        let a = shard
                            .open_named_complete(
                                Slot::new(0),
                                key("a", "r"),
                                hold,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("opens");
                        close(&mut shard, a.key.into());
                        let end = test.reader.now().monotonic + hold;
                        tick(&test, &mut shard).await;
                        assert_eq!(shard.deadline(), Some(end));
                        test.clock.sleep(Span::from_nanos(4_499_999_999)).await;
                        shard.advance();
                        assert_eq!(
                            shard.deadline(),
                            Some(end),
                            "the hold has not ended"
                        );
                        test.clock.sleep(Span::from_nanos(1)).await;
                        shard.advance();
                        assert_eq!(shard.deadline(), None);
                    });
                }

                #[test]
                fn arms_the_tick_of_a_restored_hold_at_the_carry() {
                    // Slot 2 is the last carry of `Test::shard`, so no later carry
                    // arms the tick.
                    let hold = Span::from_nanos(5_500_000_000);
                    let before = async move |_: &Test, shard: &mut Shard, w| {
                        let a = shard
                            .open_named_complete(
                                Slot::new(2),
                                key("a", "r"),
                                hold,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("opens");
                        close(shard, a.key.into());
                        shard.close_writer(w);
                    };
                    restart(
                        214,
                        before,
                        async move |test: &Test, shard: &mut Shard| {
                            let now = test.reader.now().monotonic;
                            let deadline =
                                shard.deadline().expect("the tick of the hold");
                            assert!(
                                now < deadline && deadline <= now + hold,
                                "{deadline:?}"
                            );
                        },
                    );
                }

                #[test]
                fn arms_no_tick_for_an_index_with_no_named_reader() {
                    run(200, |test| async move {
                        let mut shard = test.shard(AREA).await;
                        let complete = complete(&mut shard, Slot::new(0));
                        close(&mut shard, complete);
                        let latest = shard
                            .open_named_latest(Slot::new(0), key("a", "r"))
                            .expect("synced");
                        close(&mut shard, latest.key);
                        assert_eq!(shard.deadline(), None);
                    });
                }

                #[test]
                fn writes_the_position_at_a_takeover_by_a_latest_open() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        acked(test, shard, w).await;
                        let latest = shard
                            .open_named_latest(Slot::new(0), key("a", "r"))
                            .expect("synced");
                        close(shard, latest.key);
                    };
                    restart(204, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(caught_up(shard, a, 0).await, [seq(1, 1)]);
                    });
                }

                #[test]
                fn opens_no_reader_whose_record_finds_no_block() {
                    let before = async |test: &Test, shard: &mut Shard, w| {
                        let blocks = test.fill();
                        let refused = shard.open_named_complete(
                            Slot::new(0),
                            key("a", "r"),
                            LONG,
                            CREDIT,
                            Charge::Whole,
                        );
                        assert_eq!(refused, Err(Unopened::Unrecorded));
                        assert_eq!(shard.deadline(), None, "nothing changed");
                        drop(blocks);
                        let a = shard
                            .open_named_complete(
                                Slot::new(0),
                                key("a", "r"),
                                LONG,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("opens");
                        assert_eq!(a.replaced, None);
                        committed(test, shard, w, &[10, 20]).await;
                        let taken = taken(shard, a.key.into(), 0);
                        assert_eq!(taken, [seq(0, 1), seq(1, 1)]);
                    };
                    restart(211, before, async |_, shard: &mut Shard| {
                        let a = open(shard, "a");
                        shard.committed().await.expect("the commit ends");
                        assert_eq!(
                            caught_up(shard, a, 0).await,
                            [seq(0, 1), seq(1, 1)]
                        );
                    });
                }

                /// Opens of the longest names in a row, on the least pool, take the
                /// blocks of their entries until the commit.
                #[test]
                fn repeats_the_frames_of_opens_in_a_row_on_the_least_pool() {
                    const SMALL: usize = 62_272;
                    let open = |shard: &mut Shard, n: usize| {
                        let subject = format!("{n:03}{}", "s".repeat(252));
                        shard
                            .open_named_complete(
                                Slot::new(0),
                                key(&subject, &"r".repeat(255)),
                                LONG,
                                CREDIT,
                                Charge::Whole,
                            )
                            .map(|opened| opened.key)
                    };
                    let (mut sim, node) = create_node(213);
                    let got = sim.run_on(&node, move |node, tasks| async move {
                        let test = Test {
                            pool: Rc::new(create_pool(SMALL)),
                            ..Test::new(node, tasks)
                        };
                        let buffer = test.create_buffer(AREA, 8192, 4).await;
                        let mut shard = test.over(buffer).await;
                        assert!(shard.carry(Slot::new(0), common::key(Slot::new(0))));
                        assert!(shard.carry(Slot::new(2), common::key(Slot::new(2))));
                        let set = two_indexes();
                        let w =
                            shard.open_writer(writer("w", 1, &set)).expect("synced");
                        let opened: Vec<_> =
                            (0..7).map(|n| open(&mut shard, n)).collect();
                        let refused = opened.iter().filter(|opened| opened.is_err());
                        assert!(
                            refused.clone().all(|e| *e == Err(Unopened::Unrecorded)),
                            "{opened:?}"
                        );
                        assert!(refused.count() > 0, "the pool runs out");
                        shard.committed().await.expect("the commit ends");
                        committed(&test, &mut shard, w, &[10, 20]).await;
                        let mut got = Vec::new();
                        for opened in opened {
                            got.push(opened.map_or(0, |key| {
                                taken(&mut shard, key.into(), 0).len()
                            }));
                        }
                        got
                    });
                    let got = got.expect("the first run ends");
                    assert_eq!(
                        got,
                        [2, 2, 2, 0, 0, 0, 0],
                        "each refused open got none"
                    );
                    sim.crash(&node, sim::Crash::Power);
                    sim.run_on(&node, move |node, tasks| async move {
                        // A reopen appends its records: on the least pool, the blocks
                        // that the reads of the reader before it lend can refuse it.
                        let test = Test::new(node, tasks);
                        let buffer = test.create_buffer(AREA, 8192, 4).await;
                        let mut shard = test.over(buffer).await;
                        assert!(shard.carry(Slot::new(0), common::key(Slot::new(0))));
                        let mut caught = Vec::new();
                        for n in 0..7 {
                            let key = open(&mut shard, n).expect("opens");
                            shard.committed().await.expect("the commit ends");
                            caught.push(caught_up(&mut shard, key, 0).await.len());
                        }
                        assert_eq!(caught, got, "each opened reader gets them again");
                    })
                    .expect("the run after the cut ends");
                }

                #[test]
                fn restores_a_part_of_exactly_the_most_bytes_of_one_entry() {
                    let readers = || {
                        let long = |n: usize| {
                            key(&format!("{n}{}", "s".repeat(254)), &"r".repeat(255))
                        };
                        // 5 + 7 * 546 + 36 + 84 + 85 = 4032.
                        (0..7)
                            .map(long)
                            .chain([key(&"x".repeat(84), &"y".repeat(85))])
                            .collect::<Vec<_>>()
                    };
                    let open = |shard: &mut Shard, reader| {
                        shard
                            .open_named_complete(
                                Slot::new(0),
                                reader,
                                LONG,
                                CREDIT,
                                Charge::Whole,
                            )
                            .expect("the records fit")
                            .key
                    };
                    let before = async move |test: &Test, shard: &mut Shard, w| {
                        for reader in readers() {
                            open(shard, reader);
                        }
                        committed(test, shard, w, &[10, 20]).await;
                    };
                    restart(210, before, async move |_, shard: &mut Shard| {
                        for reader in readers() {
                            let key = open(shard, reader);
                            shard.committed().await.expect("the commit ends");
                            assert_eq!(
                                caught_up(shard, key, 0).await,
                                [seq(0, 1), seq(1, 1)]
                            );
                        }
                    });
                }

                #[test]
                fn open_panics_on_a_named_entry_of_another_version() {
                    let (mut sim, node) = create_node(201);
                    let ended = sim.run_on(&node, |node, tasks| async move {
                        let test = Test::new(node, tasks);
                        let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
                        let body = test.pool.copy(&[2, 0, 0, 0, 0]).expect("a block");
                        let record = Entry {
                            index: common::key(Slot::new(0)),
                            slot: Slot::new(0),
                            path: Path::Live,
                            first: 0,
                            samples: None,
                            stored_at: Stamp::from_nanos(1),
                            tag: 2,
                            parts: Some(body).into(),
                        };
                        buffer.append([record]).expect("the ring has room");
                        buffer.committed().await.expect("the commit ends");
                        test.over(buffer).await;
                    });
                    let Err(sim::Error::Panicked { message, .. }) = ended else {
                        panic!("the open panics: {ended:?}");
                    };
                    assert_eq!(
                        message,
                        "the named entry has version 2; this build reads version 1"
                    );
                }
            }
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
            let mut none = shard.committed();
            assert_eq!(polled(&mut none), Poll::Ready(Ok(())));
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            let handoff = handoff_to("subject-a");
            let mut commit = shard.committed();
            assert_eq!(polled(&mut commit), Poll::Pending);
            assert_eq!(find(&test.ring().await, &handoff).len(), 0);
            commit.await.expect("the commit ends");
            assert_eq!(find(&test.ring().await, &handoff).len(), 2);
        });
    }

    #[test]
    fn resolves_a_next_commit_once_a_later_handoff_is_on_disk() {
        run(2323, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let mut next = shard.next_commit();
            test.clock.sleep(SYNC).await;
            assert_eq!(polled(&mut next), Poll::Pending);
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            next.await.expect("the commit ends");
            let handoff = handoff_to("subject-a");
            assert_eq!(find(&test.ring().await, &handoff).len(), 2);
        });
    }

    #[test]
    fn resolves_a_commit_future_at_once_after_a_lost_frame() {
        run(106, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            shard.committed().await.expect("the commit ends");
            let live = frame(&test.pool, &set, &[(0, &[10, 20]), (1, &[1, 2])]);
            let blocks = test.fill();
            assert_eq!(shard.write(a, LIVE, live), Ok(&[lost(0, 0, 2)][..]));
            let mut commit = shard.committed();
            assert_eq!(polled(&mut commit), Poll::Ready(Ok(())));
            drop(blocks);
        });
    }

    #[test]
    fn resolves_a_commit_future_at_once_while_a_handoff_waits_for_room() {
        run(107, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let blocks = test.fill();
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            drop(blocks);
            let mut commit = shard.committed();
            assert_eq!(polled(&mut commit), Poll::Ready(Ok(())));
            test.clock.sleep(SYNC).await;
            let handoff = handoff_to("subject-a");
            assert_eq!(find(&test.ring().await, &handoff).len(), 0);
        });
    }

    #[test]
    fn resolves_a_commit_future_held_past_the_drop_once_the_buffer_ended() {
        run(108, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let mut none = shard.committed();
            shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            let mut commit = shard.committed();
            drop(shard);
            assert_eq!(polled(&mut none), Poll::Pending);
            assert_eq!(polled(&mut commit), Poll::Pending);
            commit.await.expect("the buffer ends");
            assert_eq!(polled(&mut none), Poll::Ready(Ok(())));
            let handoff = handoff_to("subject-a");
            assert_eq!(find(&test.ring().await, &handoff).len(), 2);
        });
    }

    #[test]
    fn gives_ok_past_the_drop_for_what_was_on_disk_when_a_later_commit_fails() {
        run(109, |test| async move {
            let set = two_indexes();
            let mut shard = test.shard(AREA).await;
            let a = shard
                .open_writer(writer("subject-a", 1, &set))
                .expect("synced");
            let mut first = shard.committed();
            assert_eq!(polled(&mut first), Poll::Pending);
            shard.committed().await.expect("the commit ends");
            let handoff = handoff_to("subject-a");
            assert_eq!(find(&test.ring().await, &handoff).len(), 2);
            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1])]);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(0, 0, 1)][..]));
            let second = shard.committed();
            test.node.fail_file(FilePath::new(RING), Operation::WriteAt);
            drop(shard);
            assert_eq!(polled(&mut first), Poll::Pending);
            let failed = env::files::Error::Io {
                path: PathBuf::from(RING),
                operation: Operation::WriteAt,
                code: 5,
            };
            assert_eq!(second.await, Err(failed));
            assert_eq!(polled(&mut first), Poll::Ready(Ok(())));
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
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
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
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            let mut shard = test.numbered(3, buffer).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
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
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            let mut shard = test.numbered(1, buffer).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
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
            let group = Group {
                index: key(Slot::new(2)),
                data: &[],
            };
            let data = [(key(Slot::new(1)), Type::Scalar(Scalar::I64))];
            let groups = [
                Group {
                    index: key(Slot::new(0)),
                    data: &data,
                },
                group,
            ];
            let mut interner = create_interner(&[0, 2], &groups);
            let set = interner.intern(&[group]);
            let other = interner.intern(&groups);
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

    /// An index type, then each kind of type, with the raw values of 3 samples.
    fn every_type() -> [(Type, Vec<u8>); 7] {
        let raw = |data_type: Type, samples: &[&[u8]]| {
            (data_type, common::raw(data_type, samples))
        };
        let element = Scalar::U64;
        let sides = types::sample::Sides {
            rows: 2,
            columns: 2,
        };
        [
            (
                Type::Scalar(Scalar::Stamp),
                [10_i64, 20, 30].map(i64::to_le_bytes).concat(),
            ),
            raw(Type::String, &[b"ab", b"", b"cde"]),
            raw(Type::Bytes, &[&[9], &[8, 7], &[6]]),
            raw(
                Type::List { element, max: 2 },
                &[
                    &4_u64.to_le_bytes(),
                    &[],
                    &[5_u64, 6].map(u64::to_le_bytes).concat(),
                ],
            ),
            (
                Type::Array {
                    element: Scalar::F32,
                    len: 2,
                },
                [0.5_f32, 1.5, 2.5, 3.5, 4.5, 5.5]
                    .map(f32::to_le_bytes)
                    .concat(),
            ),
            (
                Type::Matrix {
                    element: Scalar::I8,
                    sides,
                },
                (1..=12).collect(),
            ),
            (Type::Scalar(Scalar::Bool), vec![1, 0, 1]),
        ]
    }

    /// Writes `count` samples of each series of `series`, an index then its data, in
    /// the simulation `replay`, and checks that a reader and the stored body give them
    /// back.
    fn check_write_and_read(replay: u64, count: u32, series: Vec<(Type, Vec<u8>)>) {
        run(replay, move |test| async move {
            let data: Vec<(channel::Key, Type)> = (3..)
                .zip(&series[1..])
                .map(|(slot, &(data_type, _))| (key(Slot::new(slot)), data_type))
                .collect();
            let set = intern(&[Group {
                index: key(Slot::new(2)),
                data: &data,
            }]);
            let mut shard = test.shard(AREA).await;
            let latest = shard.open_latest(Slot::new(2)).key;
            let marked = test.now();
            let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
            let lens: Vec<_> =
                (0..).zip(&series).map(|(e, (_, v))| (e, v.len())).collect();
            let mut write =
                Draft::new(&test.pool, &set, Form::Raw, &lens).expect("room");
            for (entry, bytes) in write.iter_mut() {
                bytes.copy_from_slice(&series[entry].1);
            }
            write.set_count(0, count);
            assert_eq!(shard.write(a, LIVE, write), Ok(&[applied(2, 0, count)][..]));
            shard.committed().await.expect("the commit ends");
            let count = usize::try_from(count).expect("a small count");
            let decoded = |data_type: Type, bytes: &[u8]| -> (Type, Vec<u8>) {
                let mut out = Vec::new();
                codec::decode(data_type, count, bytes, &mut out).expect("decodes");
                (data_type, out)
            };
            let reader::Next::Frame(frame) = shard.take(latest) else {
                panic!("a frame");
            };
            let read: Vec<_> = frame
                .iter()
                .map(|(entry, bytes)| decoded(set.entries()[entry].data_type, bytes))
                .collect();
            assert_eq!(read, series);
            let ring = test.ring().await;
            let data: Vec<_> = bodies(&ring, marked)
                .into_iter()
                .filter(|(tag, _)| *tag == 0)
                .collect();
            let stored: Vec<_> = stored::read(&data[0].1)
                .map(|series| decoded(series.data_type, series.bytes))
                .collect();
            assert_eq!(stored, series);
        });
    }

    #[test]
    fn writes_and_reads_a_series_of_each_type() {
        check_write_and_read(87, 3, every_type().to_vec());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        /// At most 3 data series of 16 samples, so the frame fits one record of
        /// `BODY_MAX`.
        #[test]
        fn writes_and_reads_series_of_any_types_counts_and_ends(
            types in proptest::collection::vec(data_type(0..=3, 0..=2), 1..4),
            count in 1_u32..17,
            state in proptest::prelude::any::<u64>(),
        ) {
            let stamps = (1..=i64::from(count)).flat_map(|n| (n * 10).to_le_bytes());
            let index = (Type::Scalar(Scalar::Stamp), stamps.collect());
            let data = (1..).zip(types).map(|(n, data_type)| {
                (data_type, values(state.wrapping_add(n), count, data_type))
            });
            check_write_and_read(87, count, iter::once(index).chain(data).collect());
        }
    }

    #[test]
    fn gives_the_pool_of_its_buffer() {
        run(103, |test| async move {
            let shard = test.shard(AREA).await;
            assert!(std::ptr::eq(shard.pool(), &raw const *test.pool));
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
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            let (clock, mesh) = clock::Clock::new(test.clock.clone());
            let mut shard = Shard::open(Config {
                shard: 0,
                buffer,
                clock: mesh.clone(),
                limits: LIMITS,
            })
            .await
            .expect("opens");
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
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

    /// A monotonic clock that counts its reads.
    struct Counting {
        clock: Clock,
        reads: Arc<AtomicU64>,
    }

    impl env::clock::Driver for Counting {
        fn now(&self) -> Monotonic {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.clock.now()
        }

        fn epoch(&self) -> Instant {
            self.clock.epoch()
        }

        fn timer(&self) -> Pin<Box<dyn env::clock::Timer>> {
            Box::pin(Delegated(self.clock.sleep_until(Monotonic(0))))
        }
    }

    /// A timer of the clock that [`Counting`] reads.
    struct Delegated(env::clock::Sleep);

    impl env::clock::Timer for Delegated {
        fn poll_until(
            self: Pin<&mut Self>,
            deadline: Monotonic,
            cx: &mut Context<'_>,
        ) -> Poll<()> {
            let sleep = &mut self.get_mut().0;
            sleep.reset(deadline);
            Pin::new(sleep).poll(cx)
        }
    }

    #[test]
    fn reads_the_clock_once_to_open_write_and_close() {
        run(120, |test| async move {
            let reads = Arc::new(AtomicU64::new(0));
            let (clock, mesh) = clock::Clock::new(Clock::new(Counting {
                clock: test.clock.clone(),
                reads: Arc::clone(&reads),
            }));
            let wall = test.node.wall();
            test.tasks.spawn(async move { clock.run(wall).await });
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
            while mesh.now().mesh.is_none() {
                test.clock.sleep(Span::from_nanos(1)).await;
            }
            let mut shard = Test::with(0, buffer, mesh).await;
            assert!(shard.carry(Slot::new(0), key(Slot::new(0))));
            assert!(shard.carry(Slot::new(2), key(Slot::new(2))));
            let set = two_indexes();
            // The clock task reads it too, but `sim` polls one task at a time, so no
            // read of that task falls inside a call.
            let count = || reads.load(Ordering::Relaxed);

            let from = count();
            let key = shard.open_writer(writer("a", 1, &set)).expect("synced");
            assert_eq!(count() - from, 1, "open_writer");

            let write = frame(&test.pool, &set, &[(0, &[10]), (1, &[1]), (2, &[10])]);
            let from = count();
            assert_eq!(
                shard.write(key, LIVE, write),
                Ok(&[applied(0, 0, 1), applied(2, 0, 1)][..])
            );
            assert_eq!(count() - from, 1, "write");

            let from = count();
            shard.close_writer(key);
            assert_eq!(count() - from, 1, "close_writer");
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
