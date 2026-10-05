//! Scenarios ported from the tests of etcd/raft (Copyright 2015 The etcd Authors,
//! Apache License 2.0, see `LICENSE`). `README.md` lists each source and the changes.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod common;
mod election;
mod replication;
