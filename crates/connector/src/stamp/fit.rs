//! The fit of one device clock to mesh time.

use estimate::overlap::{Error, Overlap};
use estimate::{Drift, Measurement};
use types::time::{Interval, Monotonic, Span, Stamp};

#[cfg(test)]
#[cfg(not(loom))]
mod tests;

/// The fit of one device clock to mesh time: what each reading of that clock allows,
/// for a clock that drifts from mesh time by at most 200 ppm. A reading that
/// disagrees with the readings before it, as after a restart or a jump of the clock,
/// starts a new fit from that reading. Mesh times are taken as exact: the error of
/// the fit covers only the device clock.
#[derive(Clone, Debug)]
pub struct Fit {
    overlap: Overlap,
}

impl Default for Fit {
    /// An empty fit, which bounds no device time.
    fn default() -> Self {
        Self {
            overlap: Overlap::new(Drift::UNDISCIPLINED),
        }
    }
}

impl Fit {
    /// Adds a reading that bounds the clock from both sides: the clock gave `device`
    /// at a mesh time from `sent` to `answered`, as the answer to a read of the clock.
    pub fn between(
        &mut self,
        device: Monotonic,
        sent: Stamp,
        answered: Stamp,
    ) -> Fitted {
        let high = high(device, sent.max(answered));
        let low = low(device, sent.min(answered));
        let push = |overlap: &mut Overlap| match low {
            Some(low) => overlap.push(device, low, high),
            None => overlap.push_high(device, high),
        };
        match push(&mut self.overlap) {
            Ok(()) => Fitted::Kept,
            Err(Error::Backwards { .. } | Error::Disjoint) => self.restart(push),
        }
    }

    /// Adds a reading that bounds the clock from one side: the clock gave `device`
    /// before mesh time `arrived`, as the timestamp of a value. A reading older than
    /// the newest one that agrees with the fit adds nothing.
    pub fn before(&mut self, device: Monotonic, arrived: Stamp) -> Fitted {
        let high = high(device, arrived);
        let push = |overlap: &mut Overlap| overlap.push_high(device, high);
        match push(&mut self.overlap) {
            Ok(()) => Fitted::Kept,
            Err(Error::Backwards { .. })
                if self.mesh(device).is_none_or(|m| m.earliest <= arrived) =>
            {
                Fitted::Kept
            }
            Err(Error::Backwards { .. } | Error::Disjoint) => self.restart(push),
        }
    }

    /// The mesh time of `device`, or `None` until the readings of this fit bound the
    /// clock from both sides.
    #[must_use]
    pub fn mesh(&self, device: Monotonic) -> Option<Interval> {
        self.overlap.at(device).map(Measurement::interval)
    }

    /// Starts a new fit that holds only the reading of `push`.
    fn restart(&mut self, push: impl Fn(&mut Overlap) -> Result<(), Error>) -> Fitted {
        *self = Self::default();
        push(&mut self.overlap).expect("invariant: an empty fit holds any one reading");
        Fitted::Restarted
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

/// The offset `mesh - device`, which is at most `i64::MAX` ns.
fn offset(device: Monotonic, mesh: Stamp) -> i128 {
    i128::from(mesh.nanos()) - i128::from(device.0)
}

/// The low edge of the offset at `mesh`, or `None` when it is under the range of a
/// span: a span would claim more than the reading knows.
fn low(device: Monotonic, mesh: Stamp) -> Option<Span> {
    i64::try_from(offset(device, mesh))
        .ok()
        .map(Span::from_nanos)
}

/// The high edge of the offset at `mesh`. Under the range of a span, it is the lowest
/// span, which claims less than the reading knows.
fn high(device: Monotonic, mesh: Stamp) -> Span {
    let nanos = offset(device, mesh).max(i64::MIN.into());
    Span::from_nanos(i64::try_from(nanos).expect("invariant: mesh minus device fits"))
}
