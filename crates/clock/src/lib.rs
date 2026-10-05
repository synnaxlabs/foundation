//! Runs time source adapters and the peer exchange, feeds `estimate`, and serves mesh
//! time as an interval.

mod mesh;
pub mod source;

pub use mesh::{Clock, Reader, Status};

/// The drift bound of the node's monotonic clock.
const DRIFT: estimate::Drift = estimate::Drift::UNDISCIPLINED;
