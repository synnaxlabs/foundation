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
mod pointer;
pub mod region;
mod resolve;
pub mod retention;
pub mod subject;
pub mod time;
pub mod tree;
pub mod unit;

pub use pointer::Pointer;
