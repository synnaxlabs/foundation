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
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod bytes;
pub mod card;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod claim;
#[cfg(test)]
mod common;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod driver;
mod ed25519;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod entry;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod error;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod log;
mod member;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod message;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the streams of #471 are the first user")
)]
mod region;
pub mod status;
#[cfg(any(test, feature = "sim"))]
pub mod testing;
pub mod ticket;

pub use member::Member;
