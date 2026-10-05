use std::time::Duration;

use crate::lab::{Gap, Lab, Received};

const RATE: u64 = 1_000_000;
const HOUR: Duration = Duration::from_secs(3600);
const WRITTEN: u64 = RATE * 3600;

/// Writes at `RATE` on an edge node for one hour while its link to the cloud is cut,
/// with a disk budget that holds `budget` of samples, heals the link, and returns
/// what the Influx out connector stored and the seqs written.
fn check(budget: Duration) -> (Received, std::ops::Range<u64>) {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud");
    let edge = lab.start("edge");
    lab.limit(cloud, 1 << 40);
    let bytes = lab.budget(RATE, budget);
    lab.limit(edge, bytes);
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    lab.influx(cloud, "influx");
    lab.apply(cloud, include_str!("fixtures/edge.hcl"));
    lab.apply(cloud, include_str!("fixtures/influx.hcl"));
    lab.run(Duration::from_secs(5));
    lab.cut(edge, cloud);
    lab.write(edge, "edge.value", RATE, WRITTEN);
    lab.run(HOUR);
    lab.heal(edge, cloud);
    lab.run(HOUR);
    let stored = lab.stored("influx", "edge.value");
    let written = lab.written("edge.value");
    lab.stop();
    (stored, written)
}

#[test]
#[ignore = "waits on #295: buffer budget"]
fn a_budget_for_the_hour_delivers_every_sample_in_seq_order() {
    let (stored, written) = check(HOUR);
    assert_eq!(stored.samples, WRITTEN, "count");
    assert_eq!(stored.seqs, Some(written), "seqs");
    assert!(stored.contiguous, "seq order");
    assert_eq!(stored.gaps, [], "gaps");
}

#[test]
#[ignore = "waits on #295: buffer budget"]
fn a_budget_for_half_the_hour_delivers_one_gap_of_the_trimmed_samples() {
    let (stored, written) = check(HOUR / 2);
    let [Gap { after: 0, count }] = stored.gaps[..] else {
        panic!("one gap before every sample, got {:?}", stored.gaps);
    };
    assert_eq!(
        count + stored.samples,
        WRITTEN,
        "gap count is the trimmed samples"
    );
    assert!(
        stored.samples >= WRITTEN / 2,
        "kept {} of {WRITTEN}",
        stored.samples
    );
    assert_eq!(
        stored.seqs,
        Some(written.start + count..written.end),
        "newest kept"
    );
    assert!(stored.contiguous, "seq order");
}
