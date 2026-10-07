//! Gap lines: one line for each run of samples that the buffer trimmed before the
//! reader got them.

use std::ops::Range;

use types::name::Name;
use types::time::Stamp;

use crate::line::{Measurement, Value};

/// The measurement of the gap lines.
pub const MEASUREMENT: &str = "foundation_gaps";

/// The gap of one index of one connector. It adds up trimmed samples until the next
/// sample, then writes them as one line at that sample's stamp.
#[derive(Debug)]
pub struct Gap {
    measurement: Measurement,
    count: i64,
}

impl Gap {
    /// An empty gap, with `connector` and `index` as its tags.
    #[must_use]
    #[expect(clippy::missing_panics_doc, reason = "a name is a valid tag value")]
    pub fn new(connector: &Name, index: &Name) -> Self {
        let tags = [("connector", connector.as_str()), ("index", index.as_str())];
        Self {
            measurement: Measurement::new(MEASUREMENT, &tags, &["count"])
                .expect("invariant: a name is a valid tag value"),
            count: 0,
        }
    }

    /// Adds the trimmed samples with the seqs in `seqs`. An empty range adds
    /// nothing.
    ///
    /// # Panics
    ///
    /// When the gap passes `i64::MAX` samples, which takes 2^63 samples on one index.
    pub fn add(&mut self, seqs: Range<u64>) {
        let held = self.count;
        let count = seqs.end.saturating_sub(seqs.start);
        self.count = i64::try_from(count)
            .ok()
            .and_then(|count| held.checked_add(count))
            .unwrap_or_else(|| {
                panic!(
                    "invariant: a gap holds fewer than 2^63 samples, held {held}, \
                     added {count}"
                )
            });
    }

    /// Appends the gap's line at `stamp`, the stamp of the first sample after the
    /// gap, to `out`, and empties the gap. An empty gap appends nothing.
    ///
    /// Only `out` then holds the gap: a request built again from the samples holds
    /// it only when the reader reports the gap again.
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
