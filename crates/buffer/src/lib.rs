//! Stores each index's log durably within the disk budget (write-ahead ring, segments,
//! trimming, floors, `append`) through a per-OS driver.

mod buffer;
mod crc32c;
mod durable;
mod entry;
mod group;
mod header;
mod record;
mod tails;
mod wal;

pub use buffer::{Buffer, Commit, Config, Error};
pub use entry::Entry;
pub use group::Limit;
pub use tails::Tail;
pub use wal::{Layout, Unfit};
