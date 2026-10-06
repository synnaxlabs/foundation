use std::time::{SystemTime, UNIX_EPOCH};

use types::time::{Span, Stamp};

#[test]
fn reads_within_a_second_of_the_system_time() {
    let wall = os::wall();
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let reading = wall.now();
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let system = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the OS clock is after 1970");
    let system = Stamp::from_nanos(i64::try_from(system.as_nanos()).unwrap());
    let gap = if reading.time > system {
        reading.time - system
    } else {
        system - reading.time
    };
    assert!(gap < Span::SECOND, "read {}, system {system}", reading.time);
}

#[test]
fn gives_a_bound_under_sixteen_seconds_when_a_daemon_runs() {
    let wall = os::wall();
    #[expect(clippy::disallowed_methods, reason = "os is the crate under test")]
    let reading = wall.now();
    let error = reading.error.expect("a daemon keeps the clock");
    assert!(
        error >= Span::ZERO && error < Span::from_nanos(16_000_000_000),
        "{error}"
    );
}
