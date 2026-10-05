use std::time::Duration;

use crate::lab::{Event, Lab};

#[test]
#[ignore = "waits on #212"]
fn every_sample_carries_a_time_error_bound() {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud", 1 << 30);
    let edge = lab.start("edge", 1 << 30);
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    lab.apply(cloud, include_str!("fixtures/store_and_forward.hcl"));
    lab.write(edge, "edge.value", 1000, 10_000);
    lab.run(Duration::from_secs(20));
    let events = lab.read(cloud, "admin", "edge.value");
    assert_eq!(events.len(), 10_000, "count");
    for event in events {
        let Event::Sample(sample) = event else {
            panic!("{event:?}");
        };
        assert!(sample.error_ns.is_some(), "{sample:?}");
    }
}
