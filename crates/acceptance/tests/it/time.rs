use std::time::Duration;

use crate::lab::Lab;

/// A sanity cap on the bound in simulation. No decision sets a target yet.
const MAX_ERROR_NS: u64 = 1_000_000_000;

#[test]
#[ignore = "waits on #212"]
fn every_sample_carries_a_time_error_bound_that_holds_true_time() {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud", 1 << 30);
    let edge = lab.start("edge", 1 << 30);
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    lab.apply(cloud, include_str!("fixtures/edge.hcl"));
    lab.write(edge, "edge.value", 1000, 10_000);
    lab.run(Duration::from_secs(20));
    let times = lab.samples(cloud, "admin", "edge.time");
    let errors = lab.samples(cloud, "admin", "edge.time_error");
    let truth = lab.truth("edge.value");
    assert_eq!(times.len(), 10_000, "count");
    assert_eq!(errors.len(), times.len(), "one bound per sample");
    for ((time, error), true_ns) in times.iter().zip(&errors).zip(truth) {
        assert_eq!(error.ns, time.ns, "bound on the same timestamp");
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "u64 channel"
        )]
        let bound = error.value as u64;
        assert!(bound > 0 && bound <= MAX_ERROR_NS, "bound {bound}");
        assert!(
            time.ns.abs_diff(true_ns) <= bound,
            "{time:?} is {true_ns}±{bound}"
        );
    }
}
