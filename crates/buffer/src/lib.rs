//! Stores each index's log durably within the disk budget (write-ahead ring, segments,
//! trimming, floors, `append`, `append_at`) through a per-OS driver.
