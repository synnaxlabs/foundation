//! Defines the definitions (channels, types, units, connectors with opaque config,
//! regions, policies, open folders), the prolly tree, hashes, diffs, and
//! `spec::resolve`.

pub mod access;
pub mod channel;
pub mod compression;
pub mod connector;
pub mod data_type;
pub mod definition;
pub mod founding;
pub mod key;
pub mod node_settings;
pub mod placement;
pub mod region;
mod resolve;
pub mod retention;
pub mod subject;
pub mod time;
pub mod tree;
pub mod unit;

use types::digest::Digest;

/// The place of a region's spec in its history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pointer {
    /// The version of the spec: 0 before its first apply, and one more at each apply.
    pub version: u64,
    /// The root of the spec's tree.
    pub root: Digest,
}
