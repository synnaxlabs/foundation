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

mod applied;
mod bytes;
pub mod card;
pub mod change;
pub mod claim;
#[cfg(test)]
mod common;
mod driver;
mod entry;
mod error;
mod file;
pub mod log;
mod member;
mod message;
pub mod region;
pub mod status;
#[cfg(any(test, feature = "sim"))]
pub mod testing;
pub mod ticket;
pub mod used;

pub use driver::{Config, Ended, Mesh, Watch, founding};
pub use error::{Error, Stopped};
pub use member::Member;
