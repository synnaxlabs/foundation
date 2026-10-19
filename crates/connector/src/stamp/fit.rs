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
/// a jump of the clock, starts a new fit from that reading. A clock that goes back by
/// less than the width of a reading is not caught until a reading disagrees. A
/// `between` reading wider than 2^62 ns (146 years) bounds the clock only from above.
/// Then a high edge more than 2^62 ns over the offset `mesh - device` of the first
/// reading adds nothing, and any other edge more than 2^62 ns from it starts a new fit.
/// Mesh times are taken as exact: the error of the fit covers only the device clock.
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
    /// clock from both sides, and when the bound is past the range of a stamp.
    #[must_use]
    pub fn mesh(&self, device: Monotonic) -> Option<Interval> {
        let base = self.base?;
        let m = self.overlap.at(device)?;
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
    /// disagrees or is out of reach.
    fn read(&mut self, device: Monotonic, low: Option<i128>, high: i128) -> Fitted {
        let low = low.filter(|&low| high - low <= REACH);
        let base = *self.base.get_or_insert(high);
        let near = |edge: i128| edge.abs_diff(base) <= REACH.unsigned_abs();
        if high >= base - REACH && low.is_none_or(near) {
            // The fit already holds a high edge under the reach.
            let high = high.min(base + REACH) - base;
            match push(&mut self.overlap, device, low.map(|l| l - base), high) {
                Ok(()) => return Fitted::Kept,
                Err(Error::Disjoint) => {}
            }
        }
        *self = Self {
            base: Some(high),
            ..Self::default()
        };
        push(&mut self.overlap, device, low.map(|l| l - high), 0)
            .expect("invariant: an empty fit holds any one reading");
        Fitted::Restarted
    }
}

/// What a [`Fit`] did with a reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Fitted {
    /// The fit holds the reading with the readings before it.
    Kept,
    /// The reading disagreed with the fit or was out of its reach, and a new fit
    /// starts from it.
    Restarted,
}

/// The offset `mesh - device`.
fn offset(device: Monotonic, mesh: Stamp) -> i128 {
    i128::from(mesh.nanos()) - i128::from(device.0)
}

/// The farthest an edge gets from the base, in ns. Drift over any device time widens
/// an edge by under 2^52 ns, so an edge never reaches the range of a span.
const REACH: i128 = 1 << 62;

/// Pushes the edges `low` and `high`, each within [`REACH`] of the base.
fn push(
    overlap: &mut Overlap,
    device: Monotonic,
    low: Option<i128>,
    high: i128,
) -> Result<(), Error> {
    match low {
        Some(low) => overlap.push(device, span(low), span(high)),
        None => overlap.push_high(device, span(high)),
    }
}

/// `nanos` inside the range of an `i64` as a span.
fn span(nanos: i128) -> Span {
    Span::from_nanos(i64::try_from(nanos).expect("invariant: in reach"))
}

/// `nanos` as a stamp, stopped at the range of a stamp.
fn stamp(nanos: i128) -> Stamp {
    let nanos = nanos.clamp(i64::MIN.into(), i64::MAX.into());
    Stamp::from_nanos(i64::try_from(nanos).expect("invariant: clamped"))
}
