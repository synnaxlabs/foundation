//! Stores each index's log durably within the disk budget (write-ahead ring, segments,
//! trimming, floors, `append`) through a per-OS driver.

mod buffer;
mod crc32c;
mod entry;
mod group;
mod header;
mod log;
mod record;
mod wal;

pub use buffer::{Buffer, Commit, Config, Error, Read, Rejected, Stored};
pub use entry::{Entry, PARTS_MAX, Parts};
pub use log::{Mark, Tail};
pub use wal::{Layout, Limit, Unfit};
