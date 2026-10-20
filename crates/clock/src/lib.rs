//! Runs time source adapters and the peer exchange, feeds `estimate`, serves mesh time
//! as an interval, and gives the OS wall time and mesh time of each log line.

mod cell;
pub mod log;
mod mesh;
#[cfg_attr(not(test), expect(dead_code, reason = "the peer source is #145"))]
mod peer;
pub mod source;

pub use mesh::{Clock, Reader, Status, Time};

/// The drift bound of the node's monotonic clock.
const DRIFT: estimate::Drift = estimate::Drift::UNDISCIPLINED;
