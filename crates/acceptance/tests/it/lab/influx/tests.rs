use std::fmt::Write as _;

use super::*;

const FIRST: u64 = 100;

fn written() -> Written {
    Written {
        seqs: FIRST..FIRST + 100,
        index: "edge.time".into(),
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
    stored(&store, "influx", "edge.value", &written())
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

    /// A resend after a lost confirmation: the later gap line starts before the
    /// earlier one and ends after it.
    #[test]
    fn merges_a_gap_range_inside_a_later_gap_range() {
        let body = gap_line(FIRST + 3, 2)
            + &samples(FIRST + 3..FIRST + 4)
            + &gap_line(FIRST + 5, 5)
            + &samples(FIRST + 5..FIRST + 6);
        assert_eq!(
            check(&body),
            Received {
                samples: 2,
                seqs: Some(FIRST + 3..FIRST + 6),
                contiguous: true,
                gaps: vec![Gap { after: 0, count: 3 }, Gap { after: 1, count: 1 }],
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
            + &samples(FIRST + 2..FIRST + 3)
            + &gap_line(FIRST + 5, 1)
            + &samples(FIRST + 5..FIRST + 6);
        assert_eq!(
            check(&body),
            Received {
                samples: 3,
                seqs: Some(FIRST..FIRST + 6),
                contiguous: false,
                gaps: vec![Gap { after: 1, count: 1 }, Gap { after: 2, count: 1 }],
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
        expected = "the store holds what the lab did not write: edge.value at 1000 holds {\"value\": Float(0.5)}, not one float that is a whole number below 100"
    )]
    fn panics_on_a_value_that_is_not_a_whole_number() {
        check("edge.value value=0.5 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: edge.value at 1000 holds {\"value\": Float(-1.0)}"
    )]
    fn panics_on_a_negative_value() {
        check("edge.value value=-1 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: edge.value at 1000 holds {\"value\": Float(100.0)}"
    )]
    fn panics_on_a_value_at_the_written_count() {
        check("edge.value value=100 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: edge.value at 1000 holds {\"value\": Integer(0)}"
    )]
    fn panics_on_an_integer_value() {
        check("edge.value value=0i 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: edge.value at 1000 holds {\"a\": Float(0.0), \"value\": Float(0.0)}"
    )]
    fn panics_on_a_second_field() {
        check("edge.value value=0,a=0 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: edge.value holds seq 100 at 1010, not above seq 101 at 1000"
    )]
    fn panics_on_a_seq_below_the_seq_before_it() {
        check("edge.value value=1 1000\nedge.value value=0 1010\n");
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: edge.value holds seq 100 at 1000, not above seq 100 at 1000"
    )]
    fn panics_on_two_points_with_one_seq() {
        check("edge.value value=0 1000\nedge.value,host=a value=0 1000\n");
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1030 has path Some(\"backfill\"); backfill waits on #1270"
    )]
    fn panics_on_a_backfill_gap_line() {
        check(&gap_line(FIRST + 3, 1).replace("live", "backfill"));
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1030 has path None"
    )]
    fn panics_on_a_gap_line_with_no_path() {
        check(&gap_line(FIRST + 3, 1).replace(",path=live", ""));
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1030 has the tags {\"connector\": \"influx\", \"host\": \"a\", \"index\": \"edge.time\", \"path\": \"live\"}"
    )]
    fn panics_on_a_gap_line_with_another_tag() {
        check(&gap_line(FIRST + 3, 1).replace(",path", ",host=a,path"));
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1030 has the fields {\"count\": Integer(0)}"
    )]
    fn panics_on_a_zero_count() {
        check(&gap_line(FIRST + 3, 0));
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1030 has the fields {\"count\": Float(1.0)}"
    )]
    fn panics_on_a_float_count() {
        check(&gap_line(FIRST + 3, 1).replace("1i", "1"));
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1030 has the fields {\"count\": Integer(1), \"first\": Integer(1)}"
    )]
    fn panics_on_a_gap_line_with_another_field() {
        check(&gap_line(FIRST + 3, 1).replace("1i", "1i,first=1i"));
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1030 counts 4 seqs before seq 103, below the first written seq 100"
    )]
    fn panics_on_a_gap_range_below_the_first_written_seq() {
        check(&(gap_line(FIRST + 3, 4) + &samples(FIRST + 3..FIRST + 4)));
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1010 has no point of edge.value at its stamp"
    )]
    fn panics_on_a_gap_line_with_no_point_at_its_stamp() {
        check(
            &(samples(FIRST..FIRST + 1)
                + &gap_line(FIRST + 1, 1)
                + &samples(FIRST + 2..FIRST + 3)),
        );
    }

    #[test]
    #[should_panic(
        expected = "the store holds what the lab did not write: a gap line at 1040 has no point of edge.value at its stamp"
    )]
    fn panics_on_a_gap_line_after_the_last_point() {
        check(&(samples(FIRST..FIRST + 1) + &gap_line(FIRST + 4, 2)));
    }
}

/// The connector as #341 will write: reads in batches, a lost confirmation that
/// restarts the reader from its acked position, and trims that the reader gets as a
/// gap. The expected value is the ground truth of the run, not the fold rule.
mod connector {
    use std::collections::VecDeque;

    use connector_influx::gap;
    use connector_influx::line::{Float, Measurement, Value};
    use proptest::prelude::*;
    use types::frame::Path;

    use super::*;

    const WRITTEN: u64 = 100;

    #[derive(Debug, Clone, Copy)]
    enum Op {
        /// Reads up to `batch` samples and sends them in one request, before the
        /// requests in flight are confirmed.
        Read { batch: u64 },
        /// The oldest request in flight is confirmed, or its confirmation is lost and
        /// the reader restarts from its acked position.
        Confirm { lost: bool },
        /// The buffer drops its oldest `count` samples.
        Trim { count: u64 },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => (1..8_u64).prop_map(|batch| Op::Read { batch }),
            2 => proptest::bool::weighted(0.25).prop_map(|lost| Op::Confirm { lost }),
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
        let mut flight = VecDeque::new();
        let mut gap = new_gap();
        let mut store = Store::default();
        let mut held = vec![false; usize::try_from(WRITTEN).unwrap()];
        for &op in ops {
            match op {
                Op::Trim { count } => floor = (floor + count).min(end),
                Op::Confirm { lost } => match flight.pop_front() {
                    None => {}
                    Some(_) if lost => {
                        position = acked;
                        flight.clear();
                        gap = new_gap();
                    }
                    Some(end) => acked = end,
                },
                Op::Read { batch } => {
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
                    flight.push_back(seqs.end);
                    position = seqs.end;
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
            prop_assert_eq!(
                stored(&store, "influx", "edge.value", &written()),
                truth(&held)
            );
        }
    }
}
