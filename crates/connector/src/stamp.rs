//! The stamps of the frames that a connector writes, and the one write of each frame.

use std::iter;

use hub::home::{Outcome, Refusal, order};
use hub::writer::Failure;
use types::frame::{self, Form, Label, Path};
use types::sample::Value;
use types::time::{Interval, Span, Stamp};

mod fit;
#[cfg(test)]
#[cfg(not(loom))]
mod tests;

pub use fit::{Fit, Fitted};

/// The stamps of the frames of one writer. Each is the midpoint of the request that
/// read its samples or of the mesh time of its sample, or the nanosecond after the
/// stamp before it when that is later, so no two are equal and none goes back.
#[derive(Debug, Default)]
pub struct Stamps {
    last: Option<Stamp>,
}

impl Stamps {
    /// Runs `request`, and gives its output with its stamp: the midpoint of the mesh
    /// times of `writer` before and after it, or 1 ns after the last stamp when that
    /// is later.
    ///
    /// # Panics
    ///
    /// When the last stamp is `i64::MAX` ns.
    pub async fn request<T>(
        &mut self,
        writer: &hub::writer::Writer,
        request: impl Future<Output = T>,
    ) -> (T, Stamp) {
        let sent = writer.now();
        let output = request.await;
        let answered = writer.now();
        (output, self.next(midpoint(sent, answered)))
    }

    /// Gives the stamp of a sample whose true mesh time is within `time`, with its ends
    /// in either order, and whose value holds until at least `held`, with the error of
    /// that stamp. The stamp is the midpoint of `time`, or 1 ns after the last stamp
    /// when that is later. The error is half the width of `time`, rounded up, plus how
    /// far the stamp is past the later of the midpoint and `held`, and stops at
    /// `i64::MAX` ns.
    ///
    /// # Panics
    ///
    /// When the last stamp is `i64::MAX` ns.
    pub fn sample(&mut self, time: Interval, held: Stamp) -> (Stamp, Span) {
        let midpoint = midpoint(time.earliest, time.latest);
        let stamp = self.next(midpoint);
        let half = time
            .latest
            .nanos()
            .abs_diff(time.earliest.nanos())
            .div_ceil(2);
        let late = i128::from(stamp.nanos()) - i128::from(midpoint.max(held).nanos());
        let error = i128::from(half) + late.max(0);
        let error = i64::try_from(error).unwrap_or(i64::MAX);
        (stamp, Span::from_nanos(error))
    }

    /// Gives the nanosecond after `before`, for a frame that the home refused as
    /// `Backwards` with `before` as the stamp before it, and makes each later stamp
    /// after it.
    ///
    /// # Panics
    ///
    /// When `before` is under the last stamp, which a refusal of the frame of the last
    /// stamp never gives, or is `i64::MAX` ns.
    pub fn after(&mut self, before: Stamp) -> Stamp {
        if let Some(last) = self.last {
            assert!(
                before >= last,
                "refusal under the last stamp: {} ns < {} ns",
                before.nanos(),
                last.nanos()
            );
        }
        let stamp = before + Span::NANOSECOND;
        self.last = Some(stamp);
        stamp
    }

    /// Gives `stamp`, or the nanosecond after the last stamp when that is later.
    ///
    /// # Panics
    ///
    /// When the last stamp is `i64::MAX` ns.
    pub(crate) fn next(&mut self, stamp: Stamp) -> Stamp {
        let stamp = self
            .last
            .map_or(stamp, |last| stamp.max(last + Span::NANOSECOND));
        self.last = Some(stamp);
        stamp
    }
}

/// The hub's writer of a connector and the stamps of its frames. Each frame holds one
/// sample of each of its series, with its stamp in each index. The groups that the home
/// refuses as `Backwards` are written again once, 1 ns after the latest stamp that the
/// refusals give. No write allocates.
#[derive(Debug)]
pub(crate) struct Writer {
    hub: hub::writer::Writer,
    stamps: Stamps,
    /// Each present group of the last frame, in order, and whether the home applied it.
    groups: Vec<(u32, bool)>,
    /// The groups of the last frame that the home refused as `Backwards`, in order.
    backwards: Vec<u32>,
    /// The series of the groups that a write sends again.
    again: Vec<(usize, usize)>,
    /// `groups` of the frame sent again.
    retried: Vec<(u32, bool)>,
}

/// Why [`Writer::write`] wrote no frame, or no frame again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// The draft refused the frame, other than for a pool with no block now.
    Draft(frame::Error),
    /// The write failed.
    Write(Failure),
}

/// The class of a failed write of a run, as the error of the draft or of the write.
impl From<Error> for crate::kind::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Draft(error) => error.into(),
            Error::Write(failure) => failure.into(),
        }
    }
}

impl Writer {
    /// The writer of the frames of `hub`, with no stamp given yet. `hub` has no lease.
    pub(crate) fn new(hub: hub::writer::Writer) -> Self {
        let set = hub.set();
        let groups = set.groups().len();
        Self {
            // A write again of a set of one group sends each series of the frame.
            again: Vec::with_capacity(if groups > 1 { set.entries().len() } else { 0 }),
            groups: Vec::with_capacity(groups),
            backwards: Vec::with_capacity(groups),
            retried: Vec::with_capacity(groups),
            hub,
            stamps: Stamps::default(),
        }
    }

    /// The hub's writer, to read its key set, its entries, and its mesh time.
    pub(crate) fn hub(&self) -> &hub::writer::Writer {
        &self.hub
    }

    /// As [`Stamps::request`] on the hub's writer.
    pub(crate) async fn request<T>(
        &mut self,
        request: impl Future<Output = T>,
    ) -> (T, Stamp) {
        self.stamps.request(&self.hub, request).await
    }

    /// As [`Stamps::next`].
    pub(crate) fn next(&mut self, stamp: Stamp) -> Stamp {
        self.stamps.next(stamp)
    }

    /// As [`Stamps::sample`].
    pub(crate) fn sample(&mut self, time: Interval, held: Stamp) -> (Stamp, Span) {
        self.stamps.sample(time, held)
    }

    /// Writes one frame of `series`, as [`hub::writer::Writer::draft`] takes them, at
    /// `stamp` on the live path. Each series holds one sample. `fill` gets each entry,
    /// an index too, with the stamp of the frame and the bytes of its sample, and puts
    /// its value. Then `write` puts the stamp of the frame in each index, over what
    /// `fill` put there. The groups that the home refuses as `Backwards` are written
    /// again once, with only their series, 1 ns after the latest stamp that the
    /// refusals give, so `fill` can get an entry twice.
    ///
    /// Returns each present group in order, and whether the home applied it, at the
    /// first write or at the write again. A group that the home lost or refused, and
    /// each group of a frame for which the pool has no block now, is not applied. The
    /// slice lives until the next write.
    ///
    /// # Errors
    ///
    /// [`Error::Draft`] when the draft refuses `series`, or no block of the pool holds
    /// the frame. [`Error::Write`] with the failure of a write.
    ///
    /// # Panics
    ///
    /// When the home refuses a group as `Expired` or `Codec`, or a group written again
    /// as `Backwards`. A writer with no lease never gets `Expired`, and `codec`
    /// refuses only raw bytes that are not samples of their type, which no fill writes.
    pub(crate) fn write(
        &mut self,
        stamp: Stamp,
        series: &[(usize, usize)],
        mut fill: impl FnMut(Stamp, usize, &mut [u8]),
    ) -> Result<&[(u32, bool)], Error> {
        let Self {
            hub,
            stamps,
            groups,
            backwards,
            again,
            retried,
        } = self;
        let Some(before) = send(hub, series, stamp, &mut fill, groups, backwards)?
        else {
            return Ok(groups);
        };
        let stamp = stamps.after(before);
        let series = if backwards.len() == groups.len() {
            series
        } else {
            let entries = hub.set().entries();
            let refused = |&&(entry, _): &&(usize, usize)| {
                backwards.contains(&entries[entry].group)
            };
            again.clear();
            again.extend(series.iter().filter(refused));
            again
        };
        if let Some(last) = send(hub, series, stamp, &mut fill, retried, backwards)? {
            backwards_again(before, last);
        }
        for &(group, applied) in &*retried {
            let Ok(at) = groups.binary_search_by_key(&group, |&(group, _)| group)
            else {
                panic!("invariant: group {group}, sent again, was in the frame");
            };
            groups[at].1 = applied;
        }
        Ok(groups)
    }
}

/// Writes one frame of `series` at `stamp` on `hub`, as [`Writer::write`]. Puts each
/// present group with whether the home applied it in `groups`, and those that it
/// refused as `Backwards` in `backwards`.
///
/// Returns the latest stamp that the home gives for a group that it refuses as
/// `Backwards`.
///
/// # Errors
///
/// As [`Writer::write`].
///
/// # Panics
///
/// As [`Writer::write`], for `Expired` and `Codec`.
fn send(
    hub: &mut hub::writer::Writer,
    series: &[(usize, usize)],
    stamp: Stamp,
    fill: &mut impl FnMut(Stamp, usize, &mut [u8]),
    groups: &mut Vec<(u32, bool)>,
    backwards: &mut Vec<u32>,
) -> Result<Option<Stamp>, Error> {
    groups.clear();
    backwards.clear();
    let set = hub.set();
    let entries = set.entries();
    let mut draft = match hub.draft(Form::Raw, series) {
        Ok(draft) => draft,
        Err(frame::Error::Pool(
            block::Error::Exhausted { .. } | block::Error::Refused { .. },
        )) => {
            let index = |&&(entry, _): &&(usize, usize)| set.index(entry) == entry;
            let lost = |&(entry, _): &(usize, usize)| (entries[entry].group, false);
            groups.extend(series.iter().filter(index).map(lost));
            return Ok(None);
        }
        Err(error) => return Err(Error::Draft(error)),
    };
    for (entry, bytes) in draft.iter_mut() {
        fill(stamp, entry, bytes);
    }
    draft.set_counts(0, iter::repeat(1));
    groups.extend(draft.ranges().map(|(group, _)| (group, false)));
    for &(group, _) in &*groups {
        let index = set.groups()[to_usize(group)];
        let Some(bytes) = draft.series_mut(index) else {
            panic!("invariant: the index of group {group}, present, is in the frame");
        };
        Value::Stamp(stamp).write(bytes);
    }
    let outcomes = hub
        .write(Label::Path(Path::Live), draft)
        .map_err(Error::Write)?;
    let mut latest = None;
    for ((group, applied), outcome) in groups.iter_mut().zip(outcomes) {
        match outcome {
            Outcome::Applied { .. } => *applied = true,
            Outcome::Refused {
                refusal: Refusal::Order(order::Error::Backwards { before, .. }),
                ..
            } => {
                backwards.push(*group);
                latest = latest.max(Some(*before));
            }
            Outcome::Lost { .. }
            | Outcome::Refused {
                refusal: Refusal::Waiting | Refusal::Reserved | Refusal::Order(_),
                ..
            } => {}
            Outcome::Refused {
                refusal: refusal @ (Refusal::Expired | Refusal::Codec { .. }),
                ..
            } => panic!(
                "invariant: a writer with no lease and a fill of samples of their \
                 type are never refused, but the home gives {refusal}"
            ),
        }
    }
    Ok(latest)
}

/// The panic of a frame written again after `before`, the last stamp of its index,
/// that the home refuses as `Backwards` with `last` as the last stamp.
#[cold]
#[inline(never)]
fn backwards_again(before: Stamp, last: Stamp) -> ! {
    panic!(
        "invariant: a frame stamped after {before}, the last stamp of its index, is not \
         backwards, but the home gives {last} as last"
    )
}

/// `group` as a position.
pub(crate) fn to_usize(group: u32) -> usize {
    usize::try_from(group).expect("invariant: a usize holds a group")
}

/// The floor of the mean of `a` and `b`, with no overflow.
fn midpoint(a: Stamp, b: Stamp) -> Stamp {
    let (a, b) = (a.nanos(), b.nanos());
    // `i64::midpoint` rounds toward zero.
    Stamp::from_nanos((a >> 1) + (b >> 1) + (a & b & 1))
}
