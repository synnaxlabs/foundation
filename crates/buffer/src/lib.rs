//! Stores each index's log durably within the disk budget (write-ahead ring, segments,
//! trimming, floors, `append`) through a per-OS driver.

#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod bench;
mod buffer;
mod crc32c;
mod entry;
mod group;
mod header;
mod log;
mod read;
mod record;
mod wal;

pub use buffer::{Buffer, Commit, Config, End, Error, Rejected};
pub use entry::{Entry, PARTS_MAX, Parts};
pub use log::{Mark, Tail};
pub use read::{Read, Stored};
pub use wal::{Layout, Limit, Small, Unfit};
