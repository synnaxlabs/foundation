//! Commits past the data limit of the process. It sets a limit on the whole process,
//! so it runs in a test binary of its own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

use block::Memory as _;
use os::memory::{Error, Memory};
use rustix::param::page_size;
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
    let page = page_size();
    let memory = Memory::new(1 << 30).unwrap();
    let mut limit = process::getrlimit(Resource::Data);
    limit.current = Some(data() + (16 << 20));
    process::setrlimit(Resource::Data, limit).unwrap();
    assert_eq!(memory.commit(0, 64 << 20), Err(block::Refused));
    assert_eq!(memory.commit(0, 1 << 20), Ok(()));
    let mut offset = 1 << 20;
    while memory.commit(offset, page).is_ok() {
        offset += page;
    }
    assert_eq!(Memory::new(page).unwrap_err(), Error::Refused);
}
