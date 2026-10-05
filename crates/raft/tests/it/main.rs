//! Tests of `raft` through its public surface.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod election;
mod network;
mod replication;
