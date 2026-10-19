//! The stamps of the frames that a connector writes.

use hub::writer::Writer;
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
        writer: &Writer,
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

/// The floor of the mean of `a` and `b`, with no overflow.
fn midpoint(a: Stamp, b: Stamp) -> Stamp {
    let (a, b) = (a.nanos(), b.nanos());
    // `i64::midpoint` rounds toward zero.
    Stamp::from_nanos((a >> 1) + (b >> 1) + (a & b & 1))
}
