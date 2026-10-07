//! What a simulated InfluxDB store holds for one channel, folded back to the seqs and
//! gaps that the lab wrote.

use std::collections::BTreeMap;
use std::ops::Range;

use connector_influx::sim::{Field, Store};
use types::time::Stamp;

use super::{Gap, Received, Written};

/// What `store` holds for the channel of `written`, as `connector` writes it to the
/// data measurement `measurement`, folded by the rule of INFLUX SEQ AND GAPS:
/// - `samples` and `seqs`: the stored points, each mapped to its seq by its value.
/// - `gaps`: the union of the seq ranges of the gap lines of the connector and index,
///   minus the stored seqs, as maximal runs. A gap line ends its range at the seq of
///   the point at its stamp. `after` is the count of stored samples before the run.
/// - `contiguous`: no silent loss. Each seq from the first written seq to the last
///   stored seq is stored or in a gap range.
///
/// It does not check stamps: a sample stored at a wrong stamp that keeps the order
/// passes, and the time tests check stamps.
///
/// # Panics
///
/// When the store holds what the lab did not write: a point whose fields are not one
/// float that is a whole number below the written count, a point whose seq is not
/// above the seq of the point before it, a gap line with no point at its stamp, a gap
/// line with other tags or fields, a `path` that is not `live` (backfill waits on
/// #1270), a `count` that is not a positive integer, or a gap range that starts below
/// the first written seq.
pub(crate) fn stored(
    store: &Store,
    connector: &str,
    measurement: &str,
    written: &Written,
) -> Received {
    let reader = Reader {
        store,
        connector,
        measurement,
        written,
    };
    let mut fold = Fold {
        next: written.seqs.start,
        ranges: reader.ranges(),
        at: 0,
        first: None,
        received: Received {
            samples: 0,
            seqs: None,
            contiguous: true,
            gaps: Vec::new(),
        },
    };
    for (_, seq) in reader.points() {
        fold.sample(seq);
    }
    fold.received
}

struct Reader<'a> {
    store: &'a Store,
    connector: &'a str,
    measurement: &'a str,
    written: &'a Written,
}

impl Reader<'_> {
    /// The stamp and seq of each point of the channel, in time order. Its seqs rise.
    fn points(&self) -> impl Iterator<Item = (Stamp, u64)> {
        let mut last: Option<(Stamp, u64)> = None;
        self.store.points(self.measurement, &[]).map(move |point| {
            let seq = self.seq(point.time, point.fields);
            if let Some((stamp, before)) = last {
                assert!(
                    seq > before,
                    "the store holds what the lab did not write: {} holds seq \
                         {seq} at {}, not above seq {before} at {}",
                    self.measurement,
                    point.time.nanos(),
                    stamp.nanos()
                );
            }
            last = Some((point.time, seq));
            (point.time, seq)
        })
    }

    /// The seq of the point at `stamp` with `fields`.
    fn seq(&self, stamp: Stamp, fields: &BTreeMap<String, Field>) -> u64 {
        let seqs = &self.written.seqs;
        let count = seqs.end - seqs.start;
        let mut values = fields.values();
        let k = match (values.next(), values.next()) {
            (Some(&Field::Float(value)), None) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "the cast back refuses each value that is not a whole \
                              number in range"
                )]
                let k = value as u64;
                #[expect(clippy::cast_precision_loss, reason = "as above")]
                let back = k as f64;
                (back.to_bits() == value.to_bits() && k < count).then_some(k)
            }
            _ => None,
        };
        let Some(k) = k else {
            panic!(
                "the store holds what the lab did not write: {} at {} holds \
                 {fields:?}, not one float that is a whole number in +0..{count}",
                self.measurement,
                stamp.nanos()
            );
        };
        seqs.start + k
    }

    /// The seq ranges of the gap lines of the connector and index, sorted, with each
    /// overlapping or touching pair merged.
    fn ranges(&self) -> Vec<Range<u64>> {
        let index = self.written.index.as_str();
        let lines: Vec<(Stamp, u64)> = self
            .store
            .points(
                "foundation_gaps",
                &[("connector", self.connector), ("index", index)],
            )
            .map(|line| {
                let at = line.time.nanos();
                let path = line.tags.get("path").map(String::as_str);
                assert!(
                    path == Some("live"),
                    "the store holds what the lab did not write: a gap line at {at} \
                     has path {path:?}; backfill waits on #1270"
                );
                assert!(
                    line.tags.len() == 3,
                    "the store holds what the lab did not write: a gap line at {at} \
                     has the tags {:?}",
                    line.tags
                );
                let count = match line.fields.get("count") {
                    Some(&Field::Integer(count @ 1..)) if line.fields.len() == 1 => {
                        count
                    }
                    _ => panic!(
                        "the store holds what the lab did not write: a gap line at \
                         {at} has the fields {:?}",
                        line.fields
                    ),
                };
                (line.time, count.unsigned_abs())
            })
            .collect();
        let mut ranges: Vec<Range<u64>> = Vec::with_capacity(lines.len());
        let mut lines = lines.into_iter().peekable();
        let first = self.written.seqs.start;
        for (stamp, end) in self.points() {
            while let Some((time, count)) = lines.next_if(|&(time, _)| time <= stamp) {
                if time != stamp {
                    self.no_point(time);
                }
                let start = end
                    .checked_sub(count)
                    .filter(|start| *start >= first)
                    .unwrap_or_else(|| {
                        panic!(
                            "the store holds what the lab did not write: a gap line at \
                             {} counts {count} seqs before seq {end}, below the first \
                             written seq {first}",
                            time.nanos(),
                        )
                    });
                ranges.push(start..end);
            }
        }
        if let Some((time, _)) = lines.next() {
            self.no_point(time);
        }
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

    /// Panics on the gap line at `time`, which has no point at its stamp.
    fn no_point(&self, time: Stamp) -> ! {
        panic!(
            "the store holds what the lab did not write: a gap line at {} has no point \
             of {} at its stamp",
            time.nanos(),
            self.measurement
        );
    }
}

/// The fold of the stored seqs, in rising order, against the merged gap ranges. Each
/// range ends at a stored seq, so the last stored seq ends the fold. It needs no
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
}

#[cfg(test)]
mod tests;
