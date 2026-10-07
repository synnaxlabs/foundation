use std::fmt::Write as _;

use super::*;

const FIRST: u64 = 100;

fn record() -> Record {
    Record {
        connector: "influx".into(),
        index: "edge.time".into(),
        measurement: "edge.value".into(),
        seqs: FIRST..FIRST + 100,
        stamp: Stamp::from_nanos(1_000),
        interval: Span::from_nanos(10),
    }
}

/// The stamp of the sample with seq `seq`.
fn stamp(seq: u64) -> i64 {
    1_000 + 10 * i64::try_from(seq - FIRST).unwrap()
}

/// The data lines of the samples with the seqs in `seqs`.
fn samples(seqs: Range<u64>) -> String {
    let mut lines = String::new();
    for seq in seqs {
        writeln!(lines, "edge.value value={} {}", seq - FIRST, stamp(seq)).unwrap();
    }
    lines
}

/// A live gap line of `edge.time` before the sample with seq `seq`.
fn gap_line(seq: u64, count: i64) -> String {
    format!(
        "foundation_gaps,connector=influx,index=edge.time,path=live \
         count={count}i {}\n",
        stamp(seq)
    )
}

fn check(body: &str) -> Received {
    let mut store = Store::default();
    store.write(body.as_bytes()).unwrap();
    record().stored(&store)
}

mod stored {
    use super::*;

    #[test]
    fn holds_every_sample_with_no_gap() {
        assert_eq!(
            check(&samples(FIRST..FIRST + 3)),
            Received {
                samples: 3,
                seqs: Some(FIRST..FIRST + 3),
                contiguous: true,
                gaps: vec![],
            }
        );
    }

    #[test]
    fn holds_nothing_before_a_write() {
        assert_eq!(
            check(""),
            Received {
                samples: 0,
                seqs: None,
                contiguous: true,
                gaps: vec![],
            }
        );
    }

    #[test]
    fn gives_a_gap_before_the_first_sample() {
        let body = gap_line(FIRST + 5, 5) + &samples(FIRST + 5..FIRST + 8);
        assert_eq!(
            check(&body),
            Received {
                samples: 3,
                seqs: Some(FIRST + 5..FIRST + 8),
                contiguous: true,
                gaps: vec![Gap { after: 0, count: 5 }],
            }
        );
    }

    #[test]
    fn gives_a_gap_between_samples() {
        let body = samples(FIRST..FIRST + 2)
            + &gap_line(FIRST + 5, 3)
            + &samples(FIRST + 5..FIRST + 6);
        assert_eq!(
            check(&body),
            Received {
                samples: 3,
                seqs: Some(FIRST..FIRST + 6),
                contiguous: true,
                gaps: vec![Gap { after: 2, count: 3 }],
            }
        );
    }

    /// Seqs 15 to 17 were stored in a request whose confirmation was lost. The resend
    /// after a trim reports 10 to 17 again, but only 10 to 14 are lost.
    #[test]
    fn counts_a_stored_seq_inside_an_overlapping_gap_line_as_stored() {
        let body = samples(FIRST..FIRST + 10)
            + &gap_line(FIRST + 15, 5)
            + &samples(FIRST + 15..FIRST + 18)
            + &gap_line(FIRST + 18, 8)
            + &samples(FIRST + 18..FIRST + 20);
        assert_eq!(
            check(&body),
            Received {
                samples: 15,
                seqs: Some(FIRST..FIRST + 20),
                contiguous: true,
                gaps: vec![Gap {
                    after: 10,
                    count: 5
                }],
            }
        );
    }

    #[test]
    fn splits_a_gap_line_at_each_stored_seq() {
        let body = samples(FIRST..FIRST + 1)
            + &samples(FIRST + 3..FIRST + 4)
            + &gap_line(FIRST + 6, 5)
            + &samples(FIRST + 6..FIRST + 7);
        assert_eq!(
            check(&body),
            Received {
                samples: 3,
                seqs: Some(FIRST..FIRST + 7),
                contiguous: true,
                gaps: vec![Gap { after: 1, count: 2 }, Gap { after: 2, count: 2 }],
            }
        );
    }

    #[test]
    fn is_not_contiguous_after_a_silent_loss_between_samples() {
        let body = samples(FIRST..FIRST + 2) + &samples(FIRST + 3..FIRST + 4);
        assert_eq!(
            check(&body),
            Received {
                samples: 3,
                seqs: Some(FIRST..FIRST + 4),
                contiguous: false,
                gaps: vec![],
            }
        );
    }

    #[test]
    fn merges_touching_gap_ranges_into_one_gap() {
        let body = gap_line(FIRST + 2, 2)
            + &gap_line(FIRST + 4, 2)
            + &samples(FIRST + 4..FIRST + 5);
        assert_eq!(
            check(&body),
            Received {
                samples: 1,
                seqs: Some(FIRST + 4..FIRST + 5),
                contiguous: true,
                gaps: vec![Gap { after: 0, count: 4 }],
            }
        );
    }

    #[test]
    fn adds_no_gap_for_a_gap_range_that_ends_at_a_silent_loss() {
        let body = samples(FIRST..FIRST + 1)
            + &gap_line(FIRST + 1, 1)
            + &samples(FIRST + 2..FIRST + 3);
        assert_eq!(
            check(&body),
            Received {
                samples: 2,
                seqs: Some(FIRST..FIRST + 3),
                contiguous: false,
                gaps: vec![],
            }
        );
    }

    #[test]
    fn is_not_contiguous_after_a_silent_loss_before_the_first_sample() {
        let body = samples(FIRST + 2..FIRST + 3);
        assert_eq!(
            check(&body),
            Received {
                samples: 1,
                seqs: Some(FIRST + 2..FIRST + 3),
                contiguous: false,
                gaps: vec![],
            }
        );
    }

    #[test]
    fn is_not_contiguous_after_a_silent_loss_inside_a_gap_run() {
        let body = samples(FIRST..FIRST + 1)
            + &gap_line(FIRST + 3, 1)
            + &samples(FIRST + 3..FIRST + 4);
        assert_eq!(
            check(&body),
            Received {
                samples: 2,
                seqs: Some(FIRST..FIRST + 4),
                contiguous: false,
                gaps: vec![Gap { after: 1, count: 1 }],
            }
        );
    }

    #[test]
    fn is_not_contiguous_after_a_silent_loss_between_two_gap_runs() {
        let body = samples(FIRST..FIRST + 1)
            + &gap_line(FIRST + 2, 1)
            + &gap_line(FIRST + 5, 1)
            + &samples(FIRST + 5..FIRST + 6);
        assert_eq!(
            check(&body),
            Received {
                samples: 2,
                seqs: Some(FIRST..FIRST + 6),
                contiguous: false,
                gaps: vec![Gap { after: 1, count: 1 }, Gap { after: 1, count: 1 }],
            }
        );
    }

    #[test]
    fn gives_a_gap_line_with_no_stored_sample_after_it() {
        assert_eq!(
            check(&(samples(FIRST..FIRST + 1) + &gap_line(FIRST + 4, 2))),
            Received {
                samples: 1,
                seqs: Some(FIRST..FIRST + 1),
                contiguous: true,
                gaps: vec![Gap { after: 1, count: 2 }],
            }
        );
    }

    #[test]
    fn reads_only_the_gap_lines_of_its_index() {
        let other = format!(
            "foundation_gaps,connector=influx,index=other,path=live count=1i {}\n",
            stamp(FIRST + 1)
        );
        assert_eq!(
            check(&(samples(FIRST..FIRST + 2) + &other)),
            Received {
                samples: 2,
                seqs: Some(FIRST..FIRST + 2),
                contiguous: true,
                gaps: vec![],
            }
        );
    }

    #[test]
    fn reads_only_the_gap_lines_of_its_connector() {
        let mirror = format!(
            "foundation_gaps,connector=mirror,index=edge.time,path=live count=1i {}\n",
            stamp(FIRST + 3)
        );
        let body = samples(FIRST..FIRST + 2) + &samples(FIRST + 3..FIRST + 4) + &mirror;
        assert_eq!(
            check(&body),
            Received {
                samples: 3,
                seqs: Some(FIRST..FIRST + 4),
                contiguous: false,
                gaps: vec![],
            }
        );
    }

    #[test]
    #[should_panic(
        expected = "lab failure: no sample of edge.time was written at 1005"
    )]
    fn panics_on_a_point_between_two_stamps() {
        check("edge.value value=0 1005\n");
    }

    #[test]
    #[should_panic(expected = "lab failure: no sample of edge.time was written at 990")]
    fn panics_on_a_point_before_the_first_stamp() {
        check("edge.value value=0 990\n");
    }

    #[test]
    #[should_panic(
        expected = "lab failure: no sample of edge.time was written at 2000"
    )]
    fn panics_on_a_point_after_the_last_stamp() {
        check("edge.value value=0 2000\n");
    }

    #[test]
    #[should_panic(
        expected = "lab failure: edge.value at 1010 holds {\"value\": Float(2.0)}, \
                               not 1"
    )]
    fn panics_on_a_wrong_value() {
        check("edge.value value=2 1010\n");
    }

    #[test]
    #[should_panic(
        expected = "lab failure: edge.value at 1000 holds {\"a\": Float(0.0), \
                               \"value\": Float(0.0)}, not 0"
    )]
    fn panics_on_a_second_field() {
        check("edge.value value=0,a=0 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "lab failure: two points hold seq 100, or the points are not \
                               in seq order"
    )]
    fn panics_on_two_points_at_one_stamp() {
        check("edge.value value=0 1000\nedge.value,host=a value=0 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "lab failure: a gap line at 1030 has path Some(\"backfill\"); \
                               backfill waits on #1270"
    )]
    fn panics_on_a_backfill_gap_line() {
        check(&gap_line(FIRST + 3, 1).replace("live", "backfill"));
    }

    #[test]
    #[should_panic(expected = "lab failure: a gap line at 1030 has path None")]
    fn panics_on_a_gap_line_with_no_path() {
        check(&gap_line(FIRST + 3, 1).replace(",path=live", ""));
    }

    #[test]
    #[should_panic(
        expected = "lab failure: a gap line at 1030 has count Some(Integer(0))"
    )]
    fn panics_on_a_zero_count() {
        check(&gap_line(FIRST + 3, 0));
    }

    #[test]
    #[should_panic(
        expected = "lab failure: a gap line at 1030 has count Some(Float(1.0))"
    )]
    fn panics_on_a_float_count() {
        check(&gap_line(FIRST + 3, 1).replace("1i", "1"));
    }

    #[test]
    #[should_panic(
        expected = "lab failure: a gap line at 1030 counts 4 seqs before seq 103, \
                               below the first written seq 100"
    )]
    fn panics_on_a_gap_range_below_the_first_written_seq() {
        check(&gap_line(FIRST + 3, 4));
    }

    #[test]
    #[should_panic(
        expected = "lab failure: no sample of edge.time was written at 1005"
    )]
    fn panics_on_a_gap_line_between_two_stamps() {
        check(&gap_line(FIRST, 1).replace(" 1000", " 1005"));
    }
}

/// The connector as #341 will write: reads in batches, a lost confirmation that
/// restarts the reader from its acked position, and trims that the reader gets as a
/// gap. The expected value is the ground truth of the run, not the fold rule.
mod connector {
    use connector_influx::line::{Float, Measurement, Value};
    use proptest::prelude::*;
    use types::frame::Path;

    use super::*;

    const WRITTEN: u64 = 100;

    #[derive(Debug, Clone, Copy)]
    enum Op {
        /// Reads up to `batch` samples and writes them in one request.
        Read { batch: u64, lost: bool },
        /// The buffer drops its oldest `count` samples.
        Trim { count: u64 },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => (1..8_u64, proptest::bool::weighted(0.25))
                .prop_map(|(batch, lost)| Op::Read { batch, lost }),
            1 => (1..12_u64).prop_map(|count| Op::Trim { count }),
        ]
    }

    fn new_gap() -> gap::Gap {
        gap::Gap::new(
            &"influx".parse().unwrap(),
            &"edge.time".parse().unwrap(),
            Path::Live,
        )
    }

    /// Runs `ops` into a store, and gives the store and, for each written sample,
    /// whether a request held it.
    fn run(ops: &[Op]) -> (Store, Vec<bool>) {
        let data = Measurement::new("edge.value", &[], &["value"]).unwrap();
        let end = FIRST + WRITTEN;
        let (mut floor, mut position, mut acked) = (FIRST, FIRST, FIRST);
        let mut gap = new_gap();
        let mut store = Store::default();
        let mut held = vec![false; usize::try_from(WRITTEN).unwrap()];
        for &op in ops {
            match op {
                Op::Trim { count } => floor = (floor + count).min(end),
                Op::Read { batch, lost } => {
                    if position < floor {
                        gap.add(position..floor);
                        position = floor;
                    }
                    let seqs = position..(position + batch).min(end);
                    if seqs.is_empty() {
                        continue;
                    }
                    let mut body = Vec::new();
                    gap.line(
                        &mut body,
                        seqs.start,
                        Stamp::from_nanos(stamp(seqs.start)),
                    );
                    for seq in seqs.clone() {
                        #[expect(clippy::cast_precision_loss, reason = "below 2^53")]
                        let value = Float::new((seq - FIRST) as f64).unwrap();
                        let time = Stamp::from_nanos(stamp(seq));
                        data.line(&mut body, &[Some(Value::Float(value))], time);
                        held[usize::try_from(seq - FIRST).unwrap()] = true;
                    }
                    store.write(&body).unwrap();
                    if lost {
                        position = acked;
                        gap = new_gap();
                    } else {
                        (acked, position) = (seqs.end, seqs.end);
                    }
                }
            }
        }
        (store, held)
    }

    /// The samples that a request held, and the runs of samples up to the last held
    /// one that no request held.
    fn truth(held: &[bool]) -> Received {
        let Some(last) = held.iter().rposition(|held| *held) else {
            return Received {
                samples: 0,
                seqs: None,
                contiguous: true,
                gaps: vec![],
            };
        };
        let first = held.iter().position(|held| *held).unwrap();
        let mut received = Received {
            samples: 0,
            seqs: Some(FIRST + first as u64..FIRST + last as u64 + 1),
            contiguous: true,
            gaps: vec![],
        };
        let mut run = 0;
        for &held in &held[..=last] {
            if held {
                if run > 0 {
                    received.gaps.push(Gap {
                        after: received.samples,
                        count: run,
                    });
                    run = 0;
                }
                received.samples += 1;
            } else {
                run += 1;
            }
        }
        received
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn stores_what_the_requests_held_and_each_lost_sample_as_a_gap(
            ops in proptest::collection::vec(op(), 0..60),
        ) {
            let (store, held) = run(&ops);
            prop_assert_eq!(record().stored(&store), truth(&held));
        }
    }
}
