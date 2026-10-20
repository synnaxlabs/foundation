//! The writes of the frames of the `opcua` kind, and its drives of changes, with no
//! server, for the `kind` bench.

use types::sample::Scalar;

pub use crate::kind::{drives, writes};

/// The sample types that the kind reads, in the order of their OPC UA types.
pub const SCALARS: &[Scalar] = &crate::value::SCALARS;
