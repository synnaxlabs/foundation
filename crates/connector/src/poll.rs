//! The frames of a kind that reads values: one value of each channel that a frame
//! keeps, at the stamp of a request or of a sample.

use std::mem;

use document::diagnostic::{Code, Diagnostic};
use hub::Time;
use types::authority::Authority;
use types::frame::key_set;
use types::name::Name;
use types::sample::{Scalar, Type, Value};
use types::time::{Interval, Span, Stamp};

use crate::kind::{Context, Error};
use crate::stamp;
use crate::status;

#[cfg(test)]
#[cfg(not(loom))]
mod tests;

/// The counts that a [`Writer`] adds to: the samples that the home applied, and the
/// samples that a frame lost. The `check` of a kind that opens a `Writer` names each.
pub const COUNTS: [&str; 2] = ["in", "lost"];

const INDEX: Code = Code::new("connector.index");

/// The frames of a kind that reads values: each frame has one sample of each channel
/// that it keeps, and the stamp of the request or of the sample in each index. It
/// counts the samples that the home applied as `in`, and the samples that a frame lost
/// as `lost`. No write allocates after the first.
#[derive(Debug)]
pub struct Writer {
    stamped: stamp::Writer,
    /// The frame that keeps each value.
    all: Shape,
    /// The sample of each entry of the key set.
    fills: Vec<Fill>,
    /// The value of each channel in the frame of the last write.
    slots: Vec<Option<Value>>,
    /// The frame of the last write that left out a value.
    kept: Kept,
    applied: status::Count,
    lost: status::Count,
}

/// The series of one frame.
#[derive(Debug)]
struct Shape {
    /// Each entry of the key set in the frame and the bytes of its series.
    series: Vec<(usize, usize)>,
    /// The count of data samples of each group in the frame, by its number.
    samples: Vec<u64>,
}

/// The frame of the last write that left out a value. Each `Vec` has the capacity of
/// the frame that keeps each value from the start, so no write allocates.
#[derive(Debug)]
struct Kept {
    /// Whether the frame keeps the value of each channel, in order. Empty after a
    /// write that keeps each value.
    channels: Vec<bool>,
    /// `channels` of the write before.
    last: Vec<bool>,
    shape: Shape,
}

/// The sample of one entry of the key set in each frame.
#[derive(Clone, Copy, Debug)]
struct Fill {
    group: u32,
    /// The position of the channel of the entry, or `None` for an index, whose sample
    /// is the stamp.
    channel: Option<usize>,
    /// The type of the samples of the entry, or `None` when they are not scalars.
    scalar: Option<Scalar>,
}

impl Writer {
    /// Opens the writer of `channels` as the connector of `ctx`, with an absolute
    /// authority and no lease. `None` at the cancel of `ctx`, as [`Context::writer`]
    /// gives.
    ///
    /// # Errors
    ///
    /// The error of [`Context::writer`], and [`Error::Config`] with a diagnostic of
    /// each channel that is an index (`connector.index`), whose sample is the stamp
    /// of the request.
    ///
    /// # Panics
    ///
    /// When `check` of the kind did not name each of [`COUNTS`]. When `channels`
    /// names a channel again, which the parse of the kind refuses.
    pub async fn open<C>(
        ctx: &Context<C>,
        channels: Vec<Name>,
    ) -> Result<Option<Self>, Error> {
        let opened = ctx
            .writer(channels.clone(), Authority::ABSOLUTE, None)
            .await?;
        let Some(writer) = opened else {
            return Ok(None);
        };
        Self::new(ctx, writer, &channels).map(Some)
    }

    /// The writer of `channels` on `writer`, the hub's writer of them.
    ///
    /// # Errors
    ///
    /// As [`Self::open`], with no error of [`Context::writer`].
    ///
    /// # Panics
    ///
    /// As [`Self::open`].
    fn new<C>(
        ctx: &Context<C>,
        writer: hub::writer::Writer,
        channels: &[Name],
    ) -> Result<Self, Error> {
        let set = writer.set();
        let entries = set.entries();
        let mut channel_of = vec![None; entries.len()];
        let mut wrong = Vec::new();
        for ((channel, &entry), name) in
            writer.entries().iter().enumerate().zip(channels)
        {
            if set.index(entry) == entry {
                wrong.push(index(name));
            } else if channel_of[entry].replace(channel).is_some() {
                panic!(
                    "invariant: the parse of the kind refuses the channel `{name}` \
                     again"
                );
            }
        }
        if !wrong.is_empty() {
            return Err(Error::Config(wrong));
        }
        let mut samples = vec![0; set.groups().len()];
        let fills: Vec<_> = (entries.iter().zip(channel_of))
            .map(|(entry, channel)| {
                let key_set::Entry {
                    group, data_type, ..
                } = *entry;
                if channel.is_some() {
                    samples[stamp::to_usize(group)] += 1;
                }
                let scalar = match data_type {
                    Type::Scalar(scalar) => Some(scalar),
                    _ => None,
                };
                Fill {
                    group,
                    channel,
                    scalar,
                }
            })
            .collect();
        let width = |entry: &key_set::Entry| entry.data_type.width().unwrap_or(0);
        let series: Vec<_> = entries.iter().map(width).enumerate().collect();
        let all = Shape { series, samples };
        let [applied, lost] = COUNTS;
        Ok(Self {
            slots: vec![None; writer.entries().len()],
            kept: Kept::new(&all, writer.entries().len()),
            all,
            fills,
            applied: ctx.count(applied),
            lost: ctx.count(lost),
            stamped: stamp::Writer::new(writer),
        })
    }

    /// The sample type of each channel, in the order of `channels`.
    #[must_use]
    pub fn types(&self) -> impl ExactSizeIterator<Item = Type> + '_ {
        let hub = self.stamped.hub();
        let entries = hub.set().entries();
        (hub.entries().iter()).map(|&entry| entries[entry].data_type)
    }

    /// The mesh time of the writer's hub, as [`hub::writer::Writer::time`] gives it.
    #[must_use]
    pub fn time(&self) -> Time {
        self.stamped.hub().time()
    }

    /// Runs `request`, and gives its output with the stamp of its frame: the midpoint
    /// of the mesh times before and after it, or 1 ns after the last stamp.
    pub async fn request<T>(&mut self, request: impl Future<Output = T>) -> (T, Stamp) {
        self.stamped.request(request).await
    }

    /// Gives the stamp of a frame of samples whose true mesh time is within `time`, and
    /// whose values hold until at least `held`, with its error, as
    /// [`stamp::Stamps::sample`] gives them. Each stamp is after the stamp of each frame
    /// that this writer wrote or stamped before.
    ///
    /// # Panics
    ///
    /// When the last stamp is `i64::MAX` ns.
    pub fn sample(&mut self, time: Interval, held: Stamp) -> (Stamp, Span) {
        self.stamped.sample(time, held)
    }

    /// Writes one frame at `stamp` with the value of each channel that `values` gives,
    /// in order, and leaves out each `None`. It takes each value once, before the
    /// write. Writes no frame when it keeps none. A group that the home refuses as
    /// `Backwards` is written again once, after the stamp that the refusal gives. A
    /// frame for which the pool has no block now loses its samples.
    ///
    /// # Errors
    ///
    /// The error of a write, and [`Error::Config`] with `connector.large-frame` when
    /// the frame is larger than the largest block of the pool.
    ///
    /// # Panics
    ///
    /// When `values` gives more or fewer values than the channels, a value is not of
    /// the sample type of its channel (each value of a channel of no fixed width), or
    /// the home refuses a group as `Backwards` again.
    pub fn write(
        &mut self,
        values: impl IntoIterator<Item = Option<Value>>,
        stamp: Stamp,
    ) -> Result<(), Error> {
        let channels = self.slots.len();
        let mut slots = self.slots.iter_mut();
        let put = |value| *slots.next().unwrap_or_else(|| long(channels)) = value;
        // Not a loop of `next`: rustc keeps the `next` of a kind's `map` out of line,
        // a call per value (6 ns per sample on box1).
        values.into_iter().for_each(put);
        if slots.len() != 0 {
            short(channels - slots.len(), channels);
        }
        self.send(stamp)
    }

    /// Writes the frame of `slots` at `stamp`, as [`Writer::write`]. Apart from
    /// `write`, so that only the fill is built for each iterator.
    fn send(&mut self, stamp: Stamp) -> Result<(), Error> {
        let Self {
            stamped,
            all,
            fills,
            slots,
            kept,
            applied,
            lost,
        } = self;
        let values = &*slots;
        let shape = if values.iter().all(Option::is_some) {
            kept.channels.clear();
            all
        } else {
            kept.sort(all, fills, values);
            if kept.shape.series.is_empty() {
                return Ok(());
            }
            &kept.shape
        };
        let fill = |_, entry, bytes: &mut [u8]| {
            let Fill {
                channel, scalar, ..
            } = fills[entry];
            if let Some(channel) = channel {
                value(values, channel, scalar).write(bytes);
            }
        };
        let groups = stamped.write(stamp, &shape.series, fill)?;
        let (mut wrote, mut gone) = (0, 0);
        for &(group, applied) in groups {
            let n = shape.samples[stamp::to_usize(group)];
            if applied {
                wrote += n;
            } else {
                gone += n;
            }
        }
        applied.add(wrote);
        lost.add(gone);
        Ok(())
    }
}

impl Kept {
    /// The frame of no write yet, with the capacity of `all` and of `channels`.
    fn new(all: &Shape, channels: usize) -> Self {
        Self {
            channels: Vec::with_capacity(channels),
            last: Vec::with_capacity(channels),
            shape: Shape {
                series: Vec::with_capacity(all.series.len()),
                samples: Vec::with_capacity(all.samples.len()),
            },
        }
    }

    /// Puts in `self` the frame of `all` that keeps each value of `values` that is
    /// not `None`, when it differs from the frame of the write before.
    fn sort(&mut self, all: &Shape, fills: &[Fill], values: &[Option<Value>]) {
        mem::swap(&mut self.channels, &mut self.last);
        self.channels.clear();
        self.channels.extend(values.iter().map(Option::is_some));
        if self.channels == self.last {
            return;
        }
        let Self {
            channels, shape, ..
        } = self;
        let samples = &mut shape.samples;
        samples.clear();
        samples.resize(all.samples.len(), 0);
        for fill in fills {
            if fill.channel.is_some_and(|channel| channels[channel]) {
                samples[stamp::to_usize(fill.group)] += 1;
            }
        }
        let held = |&&(entry, _): &&(usize, usize)| {
            let Fill { group, channel, .. } = fills[entry];
            channel.map_or(samples[stamp::to_usize(group)] > 0, |channel| {
                channels[channel]
            })
        };
        shape.series.clear();
        shape.series.extend(all.series.iter().filter(held));
    }
}

/// The value of the channel at `at`, which a frame keeps, and whose samples are of
/// type `scalar`, or not scalars for `None`.
fn value(values: &[Option<Value>], at: usize, scalar: Option<Scalar>) -> Value {
    let value = values[at].unwrap_or_else(|| left_out(at));
    if Some(value.scalar()) != scalar {
        mistyped(at, value, scalar);
    }
    value
}

/// Cold and apart from `value`, so the loop of a send stores nothing per sample for
/// the message.
#[cold]
#[inline(never)]
fn left_out(at: usize) -> ! {
    panic!("invariant: a frame keeps no value of channel {at}, which is `None`")
}

/// Cold and apart from [`Writer::write`], so its fill stores nothing per value for the
/// message.
#[cold]
#[inline(never)]
fn long(channels: usize) -> ! {
    panic!("invariant: a kind gives more values than its {channels} channels")
}

/// As [`long`].
#[cold]
#[inline(never)]
fn short(given: usize, channels: usize) -> ! {
    panic!("invariant: a kind gives {given} values for {channels} channels")
}

/// As [`left_out`].
#[cold]
#[inline(never)]
fn mistyped(at: usize, value: Value, scalar: Option<Scalar>) -> ! {
    let of = Type::Scalar(value.scalar());
    let sample =
        scalar.map_or("not scalars".to_owned(), |s| Type::Scalar(s).to_string());
    panic!("invariant: the value of channel {at} is {of}, and its samples are {sample}")
}

/// The diagnostic of the channel `name`, which is an index.
fn index(name: &Name) -> Diagnostic {
    Diagnostic::new(
        INDEX,
        None,
        format!(
            "the channel `{name}` is an index, which holds the stamp of each request"
        ),
        "Write the value to a data channel".into(),
    )
}
