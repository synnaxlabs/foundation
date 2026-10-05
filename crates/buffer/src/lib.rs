//! Stores each index's log durably within the disk budget (write-ahead ring, segments,
//! trimming, floors, `append`, `append_at`) through a per-OS driver.

#[cfg(any(test, feature = "bench"))]
pub mod bench;
#[cfg_attr(
    not(any(test, feature = "bench")),
    expect(dead_code, reason = "the ring engine is the first user")
)]
mod crc32c;
#[cfg_attr(
    not(any(test, feature = "bench")),
    expect(dead_code, reason = "the ring engine is the first user")
)]
mod header;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the ring engine is the first user")
)]
mod record;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the ring engine is the first user")
)]
mod wal;
