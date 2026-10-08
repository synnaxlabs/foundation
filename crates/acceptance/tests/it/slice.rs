use std::time::Duration;

use types::time::Span;

use crate::lab::{Lab, Sample};

const COUNT: u32 = 1000;

/// What one run of the slice gave.
#[derive(Debug, PartialEq)]
struct Run {
    sent: Vec<u64>,
    /// What the reader on node B got.
    received: Vec<Sample>,
    /// What the home on node A holds.
    stored: Vec<Sample>,
    digest: u64,
}

/// Runs the slice from `key` on `link`: a reader on node B, a writer on node A, and
/// one channel whose home is node A.
fn check(key: u64, link: sim::link::Config) -> Run {
    let mut lab = Lab::new(key);
    let a = lab.start("a");
    let b = lab.start("b");
    lab.link(a, b, link);
    lab.mesh(&[a, b]);
    lab.channel(a, "a.value");
    lab.run(Duration::from_secs(1));
    let reader = lab.reader(b, "a.value");
    let values: Vec<f64> = (0..COUNT).map(|i| f64::from(i).sin() * 1e3).collect();
    lab.send(a, "a.value", &values);
    lab.run(Duration::from_secs(10));
    let run = Run {
        sent: values.iter().map(|v| v.to_bits()).collect(),
        received: lab.received(reader),
        stored: lab.samples(a, "admin", "a.value"),
        digest: lab.digest(),
    };
    lab.stop();
    run
}

/// Asserts that the home stored every value in order, with rising times, and that
/// the reader got exactly what the home stored.
fn assert_delivered(key: u64, run: &Run) {
    let stored: Vec<u64> = run.stored.iter().map(|s| s.value.to_bits()).collect();
    assert_eq!(
        stored, run.sent,
        "key {key}: home stores the values in order"
    );
    assert!(
        run.stored
            .windows(2)
            .all(|w| matches!(w, [x, y] if x.ns < y.ns)),
        "key {key}: times rise"
    );
    assert_eq!(
        run.received, run.stored,
        "key {key}: reader gets what the home stored"
    );
}

#[test]
fn a_reader_on_one_node_gets_what_a_writer_on_another_wrote_in_order() {
    let run = check(1, sim::link::Config::default());
    assert_delivered(1, &run);
    assert_eq!(
        check(1, sim::link::Config::default()),
        run,
        "one key gives one run"
    );
}

#[test]
fn a_link_that_reorders_drops_and_duplicates_changes_nothing_the_reader_gets() {
    let link = sim::link::Config {
        jitter: Span::from_nanos(2 * Span::MILLISECOND.nanos()),
        loss: 0.05,
        duplication: 0.05,
        ..sim::link::Config::default()
    };
    for key in 1..=4 {
        assert_delivered(key, &check(key, link));
    }
}
