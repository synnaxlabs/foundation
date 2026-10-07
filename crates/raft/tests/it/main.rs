//! Tests of `raft` through its public surface.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod behind;
mod change;
mod check;
mod claim;
mod config;
mod disk;
mod election;
mod hostile;
mod network;
mod replication;
