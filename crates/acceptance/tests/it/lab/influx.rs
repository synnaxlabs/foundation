//! What a simulated InfluxDB store holds for one channel, folded back to the seqs and
//! gaps that the lab wrote.

use std::ops::Range;

use connector_influx::gap;
use connector_influx::sim::{Field, Store};
use types::time::{Span, Stamp};

use super::{Gap, Received};

/// The lab's record of the samples written to one channel on the live path. Sample
/// `k` has the seq `seqs.start + k`, the stamp `stamp + k * interval`, and the value
/// `k as f64`.
#[derive(Debug, Clone)]
pub(crate) struct Record {
    /// The name of the connector that writes the channel, the `connector` tag of its
    /// gap lines.
    pub connector: String,
    /// The name of the channel's index, the `index` tag of its gap lines.
    pub index: String,
    /// The measurement of the channel's data lines.
    pub measurement: String,
    pub seqs: Range<u64>,
    /// The stamp of the first sample.
    pub stamp: Stamp,
    /// The time from one sample to the next. It is positive.
    pub interval: Span,
}

impl Record {
    /// What `store` holds for the channel, folded by the rule of INFLUX SEQ AND GAPS:
    /// - `samples` and `seqs`: the stored points, each mapped to its seq by its
    ///   stamp.
    /// - `gaps`: the union of the seq ranges of the gap lines of the connector and
    ///   index, minus the
    ///   stored seqs, as maximal runs. `after` is the count of stored samples before
    ///   the run.
    /// - `contiguous`: no silent loss. Each seq from the first written seq to the
    ///   last stored seq is stored or in a gap range.
    ///
    /// # Panics
    ///
    /// On a lab failure: a point or gap line whose stamp the record does not hold,
    /// two points at one stamp, a point whose fields are not one float with the value
    /// written at its stamp, a gap line whose `path` is not `live` (backfill waits on
    /// #1270) or whose `count` is not a positive integer, or a gap range that starts
    /// below the first written seq.
    pub(crate) fn stored(&self, store: &Store) -> Received {
        let mut fold = Fold {
            next: self.seqs.start,
            ranges: self.ranges(store),
            at: 0,
            first: None,
            received: Received {
                samples: 0,
                seqs: None,
                contiguous: true,
                gaps: Vec::new(),
            },
        };
        for point in store.points(&self.measurement, &[]) {
            let k = self.sample(point.time);
            #[expect(
                clippy::cast_precision_loss,
                reason = "a run is below 2^53 samples"
            )]
            let written = k as f64;
            assert!(
                matches!(
                    point.fields.values().collect::<Vec<_>>()[..],
                    [Field::Float(value)] if value.to_bits() == written.to_bits()
                ),
                "lab failure: {} at {} holds {:?}, not {written}",
                self.measurement,
                point.time.nanos(),
                point.fields
            );
            fold.sample(self.seqs.start + k);
        }
        fold.end()
    }

    /// The seq ranges of the gap lines of the connector and index, sorted, with each
    /// overlapping or
    /// touching pair merged.
    fn ranges(&self, store: &Store) -> Vec<Range<u64>> {
        let mut ranges: Vec<Range<u64>> = store
            .points(
                gap::MEASUREMENT,
                &[("connector", &self.connector), ("index", &self.index)],
            )
            .map(|line| {
                let path = line.tags.get("path").map(String::as_str);
                assert!(
                    path == Some("live"),
                    "lab failure: a gap line at {} has path {path:?}; backfill waits \
                     on #1270",
                    line.time.nanos()
                );
                let Some(&Field::Integer(count @ 1..)) = line.fields.get("count")
                else {
                    panic!(
                        "lab failure: a gap line at {} has count {:?}",
                        line.time.nanos(),
                        line.fields.get("count")
                    );
                };
                let end = self.seqs.start + self.sample(line.time);
                let start = end
                    .checked_sub(count.unsigned_abs())
                    .filter(|start| *start >= self.seqs.start)
                    .unwrap_or_else(|| {
                        panic!(
                            "lab failure: a gap line at {} counts {count} seqs before \
                             seq {end}, below the first written seq {}",
                            line.time.nanos(),
                            self.seqs.start
                        )
                    });
                start..end
            })
            .collect();
        ranges.sort_unstable_by_key(|range| range.start);
        let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => {
                    last.end = last.end.max(range.end);
                }
                _ => merged.push(range),
            }
        }
        merged
    }

    /// The `k` of the sample written at `stamp`.
    fn sample(&self, stamp: Stamp) -> u64 {
        let interval = self.interval.nanos();
        stamp
            .nanos()
            .checked_sub(self.stamp.nanos())
            .filter(|offset| *offset >= 0 && offset % interval == 0)
            .and_then(|offset| u64::try_from(offset / interval).ok())
            .filter(|k| *k < self.seqs.end - self.seqs.start)
            .unwrap_or_else(|| {
                panic!(
                    "lab failure: no sample of {} was written at {}",
                    self.index,
                    stamp.nanos()
                )
            })
    }
}

/// The fold of the stored seqs, in order, against the merged gap ranges. It needs no
/// memory per sample.
struct Fold {
    /// The seq after the last stored seq, or the first written seq.
    next: u64,
    ranges: Vec<Range<u64>>,
    /// The first range that may still hold a seq at or above `next`.
    at: usize,
    first: Option<u64>,
    received: Received,
}

impl Fold {
    fn sample(&mut self, seq: u64) {
        assert!(
            seq >= self.next,
            "lab failure: two points hold seq {seq}, or the points are not in seq order"
        );
        self.missing(seq);
        let first = *self.first.get_or_insert(seq);
        self.received.samples += 1;
        self.received.seqs = Some(first..seq + 1);
        self.next = seq + 1;
    }

    /// Adds the gaps in the seqs from `next` up to `end`, which no point holds. A seq
    /// there that no gap range holds is a silent loss.
    fn missing(&mut self, end: u64) {
        if self.next == end {
            return;
        }
        let mut covered = self.next;
        while let Some(range) = self.ranges.get(self.at) {
            if range.end <= self.next {
                self.at += 1;
                continue;
            }
            if range.start >= end {
                break;
            }
            let run = range.start.max(self.next)..range.end.min(end);
            if run.start > covered {
                self.received.contiguous = false;
            }
            self.received.gaps.push(Gap {
                after: self.received.samples,
                count: run.end - run.start,
            });
            covered = run.end;
            if range.end > end {
                break;
            }
            self.at += 1;
        }
        if covered < end {
            self.received.contiguous = false;
        }
    }

    /// Adds the gap ranges after the last stored seq, which are no silent loss.
    fn end(mut self) -> Received {
        while let Some(range) = self.ranges.get(self.at) {
            let start = range.start.max(self.next);
            if start < range.end {
                self.received.gaps.push(Gap {
                    after: self.received.samples,
                    count: range.end - start,
                });
            }
            self.at += 1;
        }
        self.received
    }
}

#[cfg(test)]
mod tests;
