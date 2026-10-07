//! A store of the lab's data lines holds each point in a few tens of bytes, so the
//! STORE AND FORWARD scenario fits a CI runner. This binary has no test harness, so
//! no other test grows the process while it measures.

use std::io::Write as _;

use connector_influx::sim::Store;

const POINTS: u64 = 1_000_000;

/// The most bytes a point may take: 8 of time, 8 of value, and 1 of presence, with
/// room for the allocator.
const BUDGET: u64 = 32;

/// The resident memory of this process, in bytes.
fn resident() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("Linux");
    let kib: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .expect("a VmRSS line in KiB");
    kib * 1024
}

fn main() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let mut store = Store::default();
    let mut body = Vec::new();
    let before = resident();
    for k in 0..POINTS {
        writeln!(
            body,
            "edge.value,node=edge,unit=V value={k} {}",
            1_000_000 + k * 1_000
        )
        .expect("a Vec takes each write");
        if body.len() > 1 << 20 {
            store.write(&body).expect("valid lines");
            body.clear();
        }
    }
    store.write(&body).expect("valid lines");
    drop(body);
    let grown = resident().saturating_sub(before);
    assert_eq!(
        u64::try_from(store.points("edge.value", &[]).count()),
        Ok(POINTS),
        "each point is stored"
    );
    assert!(
        grown <= BUDGET * POINTS,
        "the store took {} bytes a point, more than {BUDGET}",
        grown / POINTS
    );
}
