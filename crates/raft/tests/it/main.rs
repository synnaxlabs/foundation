//! Tests of `raft` through its public surface.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod change;
mod check;
mod config;
mod disk;
mod election;
mod network;
mod replication;
