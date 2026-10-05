//! A commit past the data limit of the process. It sets a limit on the whole process,
//! so it runs in a test binary of its own.

#![cfg(target_os = "linux")]

use block::Memory as _;
use os::memory::Memory;
use rustix::process::{self, Resource};

/// The private writable memory of the process in bytes, from `/proc/self/status`.
fn data() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmData:"))
        .and_then(|value| value.trim().strip_suffix(" kB"))
        .unwrap();
    kib.parse::<u64>().unwrap() * 1024
}

#[test]
fn a_commit_past_the_data_limit_is_refused() {
    let memory = Memory::new(1 << 30).unwrap();
    let mut limit = process::getrlimit(Resource::Data);
    limit.current = Some(data() + (16 << 20));
    process::setrlimit(Resource::Data, limit).unwrap();
    assert_eq!(memory.commit(0, 64 << 20), Err(block::Refused));
    assert_eq!(memory.commit(0, 1 << 20), Ok(()));
}
