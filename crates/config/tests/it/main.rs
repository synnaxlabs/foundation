//! Tests of `config` through its public surface, with Documents that `config_hcl`
//! reads and the kinds of `connector` crates.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

mod access;
mod connector;
mod import;
mod openssh;
mod plan;
mod private_key;
mod profile;
mod schema;
mod subject;
