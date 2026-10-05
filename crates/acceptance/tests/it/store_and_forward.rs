use std::time::Duration;

use crate::lab::{Event, Lab};

const RATE: u64 = 1_000_000;
const HOUR: u64 = 3600;
/// Bytes one buffered sample costs on disk, as an upper bound for the budget.
const SAMPLE_BYTES: u64 = 16;

/// Writes at `RATE` on an edge node for one hour while its link to the cloud is cut,
/// with a disk budget for `budget_secs` of samples, heals the link, and returns what
/// the Influx out connector received.
fn check(budget_secs: u64) -> Vec<Event> {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud", 1 << 40);
    let edge = lab.start("edge", budget_secs * RATE * SAMPLE_BYTES);
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    lab.apply(cloud, include_str!("fixtures/store_and_forward.hcl"));
    lab.run(Duration::from_secs(5));
    lab.cut(edge, cloud);
    lab.write(edge, "edge.value", RATE, RATE * HOUR);
    lab.run(Duration::from_secs(HOUR));
    lab.heal(edge, cloud);
    lab.run(Duration::from_secs(HOUR));
    lab.influx("edge.value")
}

#[test]
#[ignore = "waits on #212"]
fn a_budget_for_the_hour_delivers_every_sample_in_seq_order() {
    let events = check(HOUR);
    let seqs: Vec<u64> = events
        .iter()
        .map(|e| match e {
            Event::Sample(s) => s.seq,
            Event::Gap { count } => panic!("gap of {count}"),
        })
        .collect();
    assert_eq!(seqs.len() as u64, RATE * HOUR, "count");
    assert!(
        seqs.iter().zip(1..).all(|(&s, i)| s == seqs[0] + i - 1),
        "seq order"
    );
}

#[test]
#[ignore = "waits on #212"]
fn a_budget_for_half_the_hour_delivers_one_gap_of_the_trimmed_samples() {
    let events = check(HOUR / 2);
    let gaps: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Gap { count } => Some(*count),
            Event::Sample(_) => None,
        })
        .collect();
    let samples = events.len() as u64 - gaps.len() as u64;
    assert_eq!(gaps.len(), 1, "gaps {gaps:?}");
    assert_eq!(gaps[0], RATE * HOUR - samples, "gap count");
}
