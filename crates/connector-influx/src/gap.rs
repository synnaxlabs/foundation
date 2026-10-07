//! Gap lines: one line for each run of samples that the buffer trimmed before the
//! reader got them.

use types::time::Stamp;

use crate::line::{Error, Measurement, Value};

/// The measurement of the gap lines.
pub const MEASUREMENT: &str = "foundation_gaps";

/// The gap of one index of one connector. It adds up trimmed samples until the next
/// sample, then writes them as one line at that sample's stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gap {
    measurement: Measurement,
    count: i64,
}

impl Gap {
    /// An empty gap, with `connector` and `index` as its tags.
    ///
    /// # Errors
    ///
    /// The errors of [`Measurement::new`] for `connector` or `index`.
    pub fn new(connector: &str, index: &str) -> Result<Self, Error> {
        let tags = [("connector", connector), ("index", index)];
        Ok(Self {
            measurement: Measurement::new(MEASUREMENT, &tags, &["count"])?,
            count: 0,
        })
    }

    /// Adds `count` trimmed samples.
    ///
    /// # Panics
    ///
    /// When the gap passes `i64::MAX` samples, which takes 2^63 samples on one index.
    pub fn add(&mut self, count: u64) {
        self.count = i64::try_from(count)
            .ok()
            .and_then(|count| self.count.checked_add(count))
            .expect("a gap holds fewer than 2^63 samples");
    }

    /// Appends the gap's line at `stamp`, the stamp of the first sample after the
    /// gap, to `out`, and empties the gap. An empty gap appends nothing.
    pub fn line(&mut self, out: &mut Vec<u8>, stamp: Stamp) {
        if self.count == 0 {
            return;
        }
        let count = std::mem::take(&mut self.count);
        self.measurement
            .line(out, &[Some(Value::Integer(count))], stamp);
    }
}

#[cfg(test)]
mod tests;
