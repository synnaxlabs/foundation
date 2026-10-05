use std::time::Duration;

use crate::lab::{Lab, Sample};

const COUNT: u32 = 1000;

/// Runs the slice from `key`: a reader on node B, a writer on node A, and one channel
/// whose home is node A. Returns the values written and the samples the reader got.
fn check(key: u64) -> (Vec<f64>, Vec<Sample>) {
    let mut lab = Lab::new(key);
    let a = lab.start("a");
    let b = lab.start("b");
    lab.mesh(&[a, b]);
    lab.channel(a, "a.value");
    lab.run(Duration::from_secs(1));
    let reader = lab.reader(b, "a.value");
    let values: Vec<f64> = (0..COUNT).map(|i| f64::from(i).sin() * 1e3).collect();
    lab.send(a, "a.value", &values);
    lab.run(Duration::from_secs(5));
    let received = lab.received(reader);
    lab.stop();
    (values, received)
}

#[test]
#[ignore = "waits on #462"]
fn a_reader_on_one_node_gets_what_a_writer_on_another_wrote_in_order() {
    let (values, received) = check(1);
    let got: Vec<u64> = received.iter().map(|s| s.value.to_bits()).collect();
    let sent: Vec<u64> = values.iter().map(|v| v.to_bits()).collect();
    assert_eq!(got, sent, "values in order");
    assert!(
        received
            .windows(2)
            .all(|w| matches!(w, [x, y] if x.ns < y.ns)),
        "times rise"
    );
    assert_eq!(check(1).1, received, "one key gives one run");
}
