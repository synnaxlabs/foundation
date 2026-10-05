//! Agrees per region, through `raft`, on spec pointers, delegations, and runtime state
//! (membership, node leases, homes, seq blocks, index history, secret ciphertexts,
//! tickets, versions, rollout lock, format flag); serves snapshots, watches, effective
//! settings, and the changes channels.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the driver of #471 is the first user")
)]
mod entry;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the driver of #471 is the first user")
)]
mod log;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the driver of #471 is the first user")
)]
mod message;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the driver of #471 is the first user")
)]
mod region;
