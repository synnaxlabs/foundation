//! The indexes one shard carries: their writers, each frame from split to one buffer
//! append, and their readers.

use std::fmt;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use buffer::{Buffer, Entry};
use delivery::Reader;
use types::channel::Slot;
use types::frame::key_set::{self, KeySet};
use types::frame::{self, Draft, Frame, Label, Path};
use types::hash;
use types::time::{Monotonic, Span, Stamp};

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
/// use home::{Config, Outcome, Shard, order, reader::Next, writer};
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
/// let charge = home::reader::complete::Charge::Whole;
/// let reader = shard.open_complete(index, 1 << 20, charge);
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
    /// gap in the log. The gap is durable only when a later live entry of the index,
    /// with samples or a handoff, is on disk: a restart before that gives the next
    /// frame the same seq. After a [`Shard::shed`] and a [`Shard::carry`] of the index,
    /// the next frame gets the same seq only when no later live entry was appended.
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

/// Resolves when every group applied and every handoff appended before
/// [`Shard::committed`] is on disk, or with the error that ended the buffer first. It
/// does not borrow the shard, and it holds the shard's ring open until it drops. Held
/// past the drop of the shard, it resolves only once the buffer ended, with an error
/// or with none. Its result is only for what was appended before the call.
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
    /// from its tail in the buffer. It does nothing when the shard carries `slot`
    /// already.
    pub fn carry(&mut self, slot: Slot) {
        if self.places.contains_key(&slot) {
            return;
        }
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
        self.places.insert(slot, place);
        self.indexes.push(index);
        self.readers.carry(place, slot, live.seq);
    }

    /// Stops carrying the index at `slot`: its control gate goes, and with it a handoff
    /// that waits for room. Each named reader of the index stops holding its position.
    /// Its frames stay in the buffer, so a later [`carry`](Self::carry) of `slot`
    /// continues each path from its tail in the buffer: the last entry appended, on
    /// disk or not.
    ///
    /// # Panics
    ///
    /// If the shard does not carry `slot`, or a writer or a reader is open on it.
    pub fn shed(&mut self, slot: Slot) {
        let place = self.place(slot);
        let mut claims = self.writers.values().flat_map(|session| &session.claims);
        assert!(
            claims.all(|claim| claim.place != place),
            "a writer is open on the index at {slot:?}"
        );
        let last = self.indexes.len() - 1;
        self.places.remove(&slot);
        self.indexes.swap_remove(place);
        self.readers.shed(place);
        if place != last {
            let moved = self.places.values_mut().find(|at| **at == last);
            *moved.expect("invariant: the last place has a slot") = place;
            let claims = self.writers.values_mut().flat_map(|s| &mut s.claims);
            for claim in claims.filter(|claim| claim.place == last) {
                claim.place = place;
            }
        }
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
        let (now, mesh) = self.now().ok_or(writer::Error::Unsynced)?;
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
    /// A handoff with no room decides before the size: the groups with samples of a
    /// live frame are lost, and a backfill frame gets [`Error::Full`]. [`Error::Disk`]
    /// after a failed commit, before any other error but [`Error::Resend`].
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
        // An empty append still reports a failed commit.
        let appended = room(self.buffer.append(entries.drain(..)));
        let room = match (recorded, appended, made, path) {
            (Err(error), ..) | (Ok(_), Err(error), ..) => Err(error),
            (Ok(true), Ok(_), Err(block::Error::TooLarge { .. }), _) => {
                Err(Error::Large)
            }
            (Ok(true), Ok(true), Ok(()), _) => Ok(true),
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

    /// Opens an unnamed complete reader on the index at `slot`, with a credit of
    /// `limit_bytes`. From the index's live tail on, it gets each live frame with
    /// samples after the commit that holds it, while the bytes it has spent are below
    /// its credit: a frame spends what `charge` says. A frame that finds the credit
    /// spent waits for a [`grant`](Self::grant), and so does each later frame. A frame
    /// that still waits when a later commit releases frames of the index is a miss:
    /// the reader gets neither it nor a later frame, and [`take`](Self::take) gives
    /// the frames before it, then [`Next::Behind`](reader::Next::Behind).
    /// [`woken`](Self::woken) names the reader once for a miss with no frame to take,
    /// and not for a miss while frames wait to be taken. The home does not read a
    /// missed frame back from disk yet. Close the reader and open a new one. The new
    /// one starts at the live tail of its open, so the frames from the miss to there
    /// reach neither reader.
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
    ) -> reader::complete::Key {
        let place = self.place(slot);
        let live = self.indexes[place].live_tail();
        let session = self.readers.open_complete(place, live, limit_bytes, charge);
        reader::complete::Key { slot, session }
    }

    /// Opens a complete session for the named reader `reader` on the index at `slot`,
    /// as [`open_complete`](Self::open_complete) does, but it starts at the reader's
    /// last acked position while the reader holds one: its session is open, or it
    /// closed less than its `hold` ago. Else it starts at the live tail. It takes over
    /// the open session of the same reader, in either mode. After a close, the reader
    /// holds its position for `hold`, in mesh time. When the position is below the
    /// frames that memory keeps, the reader gets no frame: [`take`](Self::take) gives
    /// [`Next::Behind`](reader::Next::Behind).
    ///
    /// # Errors
    ///
    /// [`reader::Unsynced`] before the node first has mesh time.
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
    ) -> Result<reader::Opened<reader::complete::Key>, reader::Unsynced> {
        let place = self.place(slot);
        let (_, now) = self.now().ok_or(reader::Unsynced)?;
        let live = self.indexes[place].live_tail();
        let named = Reader::Named { reader, hold };
        let opened = self.readers.open_named_complete(
            place,
            named,
            live,
            limit_bytes,
            charge,
            now,
        );
        Ok(reader::Opened {
            key: reader::complete::Key {
                slot,
                session: opened.key,
            },
            replaced: opened.replaced.map(|session| reader::Key { slot, session }),
        })
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
        let (_, now) = self.now().ok_or(reader::Unsynced)?;
        let opened = self.readers.open_named_latest(place, reader, now);
        Ok(reader::Opened {
            key: reader::Key {
                slot,
                session: opened.key.into(),
            },
            replaced: opened.replaced.map(|session| reader::Key { slot, session }),
        })
    }

    /// Records that the complete reader `key` has each sample of its index below
    /// `position`. A named reader that opens again starts there. An ack to a closed
    /// reader changes nothing.
    ///
    /// # Errors
    ///
    /// [`reader::Error::Ack`] when the reader is open and `position` moves back.
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
    /// when none waits or the reader is closed. A complete reader that misses a frame
    /// ([`open_complete`](Self::open_complete)) gets the frames before it, then
    /// [`Next::Behind`](reader::Next::Behind).
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`, or does not carry the index of `key`.
    #[must_use]
    pub fn take(&mut self, key: reader::Key) -> reader::Next {
        self.readers.take(self.place(key.slot), key.session)
    }

    /// Closes the reader `key`. Its waiting frames do not go out, and
    /// [`woken`](Self::woken) does not name it. A named complete reader holds its
    /// position for its `hold` after the close. A close of a closed reader changes
    /// nothing.
    ///
    /// # Panics
    ///
    /// If the shard never gave `key`, or does not carry the index of `key`.
    pub fn close_reader(&mut self, key: reader::Key) {
        let now = self.now().map(|(_, now)| now);
        self.readers.close(self.place(key.slot), key.session, now);
    }

    /// Replaces `keys` with the readers to wake since the last call, each once, in slot
    /// order and with the latest readers of an index first. Call it after each write,
    /// because a latest reader gets a frame before its commit, and after each commit.
    /// Complete readers first get the live frames now on disk. A key is a hint: take
    /// from each until [`take`](Self::take) gives [`Next::Empty`](reader::Next::Empty)
    /// or [`Next::Behind`](reader::Next::Behind). When a commit ended since the last
    /// call, it reads each index with live frames queued for complete readers; else it
    /// reads none. Called so, with the same `keys` each time, a call allocates only
    /// when it gives more keys than each call before, or when more frames wait for one
    /// complete reader than have waited for that reader before.
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
    /// Before the node first has mesh time. No writer or named reader opens before it,
    /// and mesh time stays once known.
    fn time(&self) -> (Monotonic, Stamp) {
        let now = self.now();
        now.expect("invariant: mesh time stays once known")
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
        let (claim, entry) = session.claim(group);
        let slot = entry.slot;
        let index = &mut indexes[claim.place];
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::Waker;
    use std::time::Instant;

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
    use crate::common::{create_interner, create_pool, data_type, key, values};
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
            Self::with(shard, buffer, self.reader.clone())
        }

        /// A shard over the ring of the node that never has mesh time, with no index
        /// carried.
        async fn unsynced(&self) -> Shard {
            let buffer = self.create_buffer(AREA, BODY_MAX, 4).await;
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
            (shard, create_interner().intern(&groups))
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
        create_interner().intern(&[
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
        create_interner().intern(&[Group {
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
        shard.open_complete(slot, CREDIT, Charge::Whole).into()
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
                end @ (reader::Next::Empty | reader::Next::Behind) => {
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
    /// If `reader` gets [`reader::Next::Behind`].
    #[track_caller]
    fn taken(shard: &mut Shard, reader: reader::Key, group: u32) -> Vec<Range> {
        let (frames, end) = drain(shard, reader);
        let ranges = ranges(&frames, group);
        assert!(
            matches!(end, reader::Next::Empty),
            "behind after {ranges:?}"
        );
        ranges
    }

    /// The seq of the index group `group` of each frame that `reader` takes before
    /// [`reader::Next::Behind`].
    ///
    /// # Panics
    ///
    /// If `reader` gets [`reader::Next::Empty`].
    #[track_caller]
    fn missed(
        shard: &mut Shard,
        reader: impl Into<reader::Key>,
        group: u32,
    ) -> Vec<Range> {
        let (frames, end) = drain(shard, reader);
        let ranges = ranges(&frames, group);
        assert!(
            matches!(end, reader::Next::Behind),
            "not behind after {ranges:?}"
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
            shard.carry(Slot::new(0));
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
            shard.carry(Slot::new(1));
            let set = create_interner().intern(&[Group {
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
            let zero = create_interner().intern(&[Group {
                index: key(Slot::new(0)),
                data: &[],
            }]);
            let two = create_interner().intern(&[Group {
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
            shard.carry(Slot::new(0));
            let set = create_interner().intern(&[Group {
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
            let set = create_interner().intern(&[Group {
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
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole);
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
                let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
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
        fn gives_a_complete_reader_a_frame_that_waits_for_credit_at_a_grant() {
            run(37, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole);
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
        fn misses_a_frame_that_waits_for_credit_at_the_next_commit() {
            run(37, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole);
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
                assert_eq!(missed(&mut shard, reader, 0), []);
                shard.grant(session, CREDIT);
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(missed(&mut shard, session, 0), []);
            });
        }

        #[test]
        fn gives_a_frame_that_waits_for_credit_on_a_quiet_index_after_a_grant() {
            run(39, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole);
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
                let session = shard.open_complete(Slot::new(0), index + 1, places);
                let reader = reader::Key::from(session);
                let places = Charge::Places([Slot::new(1), Slot::new(0)].into());
                let data = shard.open_complete(Slot::new(0), 2 * index + 1, places);
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
                for key in [reader, data.into()] {
                    assert_eq!(missed(&mut shard, key, 0), []);
                }
            });
        }

        #[test]
        fn names_a_complete_reader_once_when_it_misses_a_frame_with_none_waiting() {
            run(110, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_complete(Slot::new(0), 1, Charge::Whole).into();
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
                assert_eq!(missed(&mut shard, reader, 0), []);
                write(&test, &mut shard, a, &[40]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), []);
                assert_eq!(missed(&mut shard, reader, 0), []);
            });
        }

        #[test]
        fn does_not_name_a_complete_reader_that_misses_a_frame_while_one_waits() {
            run(111, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let reader = shard.open_complete(Slot::new(0), 1, Charge::Whole).into();
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10]);
                shard.committed().await.expect("the commit ends");
                assert_eq!(woken(&mut shard), [reader]);
                for stamp in [20, 30] {
                    write(&test, &mut shard, a, &[stamp]);
                    shard.committed().await.expect("the commit ends");
                    assert_eq!(woken(&mut shard), []);
                }
                assert_eq!(missed(&mut shard, reader, 0), [seq(0, 1)]);
                assert_eq!(woken(&mut shard), []);
            });
        }

        #[test]
        fn does_not_count_a_frame_of_no_samples_as_a_miss_of_a_complete_reader() {
            run(104, |test| async move {
                let set = two_indexes();
                let mut shard = test.shard(AREA).await;
                let session = shard.open_complete(Slot::new(2), 1, Charge::Whole);
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
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole);
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
                let session = shard.open_complete(Slot::new(0), 1, Charge::Whole);
                close(&mut shard, session.into());
                shard.grant(session, CREDIT);
                let after = shard.open_complete(Slot::new(0), 1, Charge::Whole);
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
                let _key = shard.open_complete(Slot::new(3), CREDIT, Charge::Whole);
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
                let reader = shard.open_complete(Slot::new(2), CREDIT, Charge::Whole);
                let other = reader::complete::Key {
                    slot: Slot::new(3),
                    ..reader
                };
                shard.grant(other, CREDIT + 1);
            });
        }

        /// The key set of the index at slot 2 alone.
        fn only_two() -> Arc<KeySet> {
            create_interner().intern(&[Group {
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
                let old = shard.open_complete(Slot::new(0), 1, Charge::Whole);
                let a = shard.open_writer(writer("a", 1, &set)).expect("synced");
                write(&test, &mut shard, a, &[10, 20]);
                shard.close_writer(a);
                shard.close_reader(old.into());
                shard.shed(Slot::new(0));
                shard.carry(Slot::new(0));
                let new = shard.open_complete(Slot::new(0), 1, Charge::Whole);
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
            create_interner().intern(&groups)
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
                shard.carry(Slot::new(0));
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
        fn panics_at_the_shed_of_an_index_it_does_not_carry() {
            check_not_carried(135, |shard| shard.shed(Slot::new(3)));
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

        /// The ring is full, so the handoff of the close waits, and the shed drops it.
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
                // No public outcome shows a waiting handoff before the shed drops it.
                assert!(shard.indexes[0].handoff().is_some(), "the handoff waits");
                shard.shed(Slot::new(0));
                shard.carry(Slot::new(0));
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
                shard.carry(Slot::new(0));
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
                shard.carry(Slot::new(0));
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
                shard.carry(Slot::new(0));
                let b = shard.open_writer(writer("b", 1, &set)).expect("synced");
                let next = frame(&test.pool, &set, &[(0, &[40]), (1, &[4])]);
                assert_eq!(shard.write(b, LIVE, next), Ok(&[applied(0, 1, 1)][..]));
            });
        }

        mod named {
            use super::*;
            use crate::reader::{Error, Opened, Position, Unsynced, complete, named};

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
                    shard.carry(Slot::new(0));
                    let complete = shard.open_named_complete(
                        Slot::new(0),
                        key("s", "r"),
                        HOLD,
                        CREDIT,
                        Charge::Whole,
                    );
                    assert_eq!(complete, Err(Unsynced));
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
                    let error = Error::Ack {
                        from: live(2),
                        to: live(1),
                    };
                    assert_eq!(shard.ack(first.key, live(1)), Err(error));
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
                    assert_eq!(missed(&mut shard, second.key, 0), []);
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
            fn ends_behind_after_an_open_again_below_the_live_frames() {
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
                    committed(&test, &mut shard, a, &[50]).await;
                    assert_eq!(missed(&mut shard, second.key, 0), []);
                });
            }

            /// As `ends_behind_after_an_open_again_below_the_live_frames`, with a shed
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
                    shard.carry(Slot::new(0));
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
                    assert_eq!(missed(&mut shard, second.key, 0), []);
                });
            }

            #[test]
            fn takes_over_a_latest_session_with_a_latest_open() {
                run(127, |test| async move {
                    let set = two_indexes();
                    let mut shard = test.shard(AREA).await;
                    let a = shard.open_writer(writer("w", 1, &set)).expect("synced");
                    let first = latest(&mut shard, "a", "r");
                    write(&test, &mut shard, a, &[10]);
                    let second = latest(&mut shard, "a", "r");
                    assert_eq!(second.replaced, Some(first.key));
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
                    assert_eq!(missed(&mut shard, second.key, 0), []);
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
                    assert_eq!(missed(&mut shard, second.key, 0), []);
                });
            }

            #[test]
            fn stays_behind_at_each_open_within_its_hold() {
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
                    assert_eq!(missed(&mut shard, last, 0), []);
                });
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
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
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
            let buffer = test.create_buffer(AREA, BODY_MAX, 4).await;
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
            let mut interner = create_interner();
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

    /// An index type, then each kind of type, with the raw values of 3 samples. A
    /// variable series is its ends, zeros to the start of its elements, then them.
    fn every_type() -> [(Type, Vec<u8>); 7] {
        let le = |values: &[u32]| -> Vec<u8> {
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect()
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
            (Type::String, [le(&[2, 2, 5]), b"abcde".to_vec()].concat()),
            (Type::Bytes, [le(&[1, 3, 4]), vec![9, 8, 7, 6]].concat()),
            (
                Type::List { element, max: 2 },
                [
                    le(&[1, 1, 3, 0]),
                    [4_u64, 5, 6].map(u64::to_le_bytes).concat(),
                ]
                .concat(),
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
            let set = create_interner().intern(&[Group {
                index: key(Slot::new(2)),
                data: &data,
            }]);
            let mut shard = test.shard(AREA).await;
            let latest = shard.open_latest(Slot::new(2));
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
                let len = codec::validate(data_type, count, bytes).expect("valid");
                let mut out = vec![0; len];
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
            let mut shard = Test::with(0, buffer, mesh);
            shard.carry(Slot::new(0));
            shard.carry(Slot::new(2));
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
