//! The fit of one device clock to mesh time.

use estimate::Drift;
use estimate::overlap::{Error, Overlap};
use types::time::{Interval, Monotonic, Span, Stamp};

#[cfg(test)]
#[cfg(not(loom))]
mod tests;

/// The fit of one device clock to mesh time: what each reading of that clock allows,
/// for a clock that drifts from mesh time by at most 200 ppm. Readings come in any
/// order. A reading that disagrees with the readings before it, as after a restart or
/// a jump of the clock, starts a new fit from that reading. Mesh times are taken as
/// exact: the error of the fit covers only the device clock.
#[derive(Clone, Debug)]
pub struct Fit {
    overlap: Overlap,
    /// The offset `mesh - device` of the first reading, which `overlap` holds the
    /// offsets from, or `None` before it.
    base: Option<i128>,
}

impl Default for Fit {
    /// An empty fit, which bounds no device time.
    fn default() -> Self {
        Self {
            overlap: Overlap::new(Drift::UNDISCIPLINED),
            base: None,
        }
    }
}

impl Fit {
    /// Adds a reading that bounds the clock from both sides: the clock gave `device`
    /// at a mesh time from `sent` to `answered`, in either order, as the answer to a
    /// read of the clock.
    pub fn between(
        &mut self,
        device: Monotonic,
        sent: Stamp,
        answered: Stamp,
    ) -> Fitted {
        let low = offset(device, sent.min(answered));
        self.read(device, Some(low), offset(device, sent.max(answered)))
    }

    /// Adds a reading that bounds the clock from one side: the clock gave `device`
    /// before mesh time `arrived`, as the timestamp of a value.
    pub fn before(&mut self, device: Monotonic, arrived: Stamp) -> Fitted {
        self.read(device, None, offset(device, arrived))
    }

    /// The mesh time of `device`, or `None` until the readings of this fit bound the
    /// clock from both sides, when drift since them makes the bound 36500 days or
    /// wider, and when the bound is past the range of a stamp.
    #[must_use]
    pub fn mesh(&self, device: Monotonic) -> Option<Interval> {
        let base = self.base?;
        let m = self.overlap.at(device).filter(|m| m.known())?;
        let time = i128::from(device.0) + base + i128::from(m.offset().nanos());
        let error = i128::from(m.error().nanos());
        let (earliest, latest) = (time - error, time + error);
        // Mesh time is a stamp, so a bound past the range on one side loses nothing.
        if earliest > i64::MAX.into() || latest < i64::MIN.into() {
            return None;
        }
        Some(Interval {
            earliest: stamp(earliest),
            latest: stamp(latest),
        })
    }

    /// Adds a reading of offsets from `low` to `high`, or a new fit from it when it
    /// disagrees.
    fn read(&mut self, device: Monotonic, low: Option<i128>, high: i128) -> Fitted {
        let base = *self.base.get_or_insert(high);
        match push(
            &mut self.overlap,
            device,
            low.map(|l| l - base),
            high - base,
        ) {
            Ok(()) => Fitted::Kept,
            Err(Error::Disjoint) => {
                *self = Self {
                    base: Some(high),
                    ..Self::default()
                };
                push(&mut self.overlap, device, low.map(|l| l - high), 0)
                    .expect("invariant: an empty fit holds any one reading");
                Fitted::Restarted
            }
        }
    }
}

/// What a [`Fit`] did with a reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Fitted {
    /// The fit holds the reading with the readings before it.
    Kept,
    /// The reading disagreed with the fit, and a new fit starts from it.
    Restarted,
}

/// The offset `mesh - device`.
fn offset(device: Monotonic, mesh: Stamp) -> i128 {
    i128::from(mesh.nanos()) - i128::from(device.0)
}

/// Pushes the edges `low` and `high` from the base. An edge past the range of a span
/// stops at that range when that claims less, and is left out when it would claim
/// more.
fn push(
    overlap: &mut Overlap,
    device: Monotonic,
    low: Option<i128>,
    high: i128,
) -> Result<(), Error> {
    let low = low
        .filter(|&low| low >= i64::MIN.into())
        .map(|low| span(low.min(i64::MAX.into())));
    let high = (high <= i64::MAX.into()).then(|| span(high.max(i64::MIN.into())));
    match (low, high) {
        (Some(low), Some(high)) => overlap.push(device, low, high),
        (Some(low), None) => overlap.push_low(device, low),
        (None, Some(high)) => overlap.push_high(device, high),
        (None, None) => Ok(()),
    }
}

/// `nanos` inside the range of an `i64` as a span.
fn span(nanos: i128) -> Span {
    Span::from_nanos(i64::try_from(nanos).expect("invariant: in range"))
}

/// `nanos` as a stamp, stopped at the range of a stamp.
fn stamp(nanos: i128) -> Stamp {
    let nanos = nanos.clamp(i64::MIN.into(), i64::MAX.into());
    Stamp::from_nanos(i64::try_from(nanos).expect("invariant: clamped"))
}
