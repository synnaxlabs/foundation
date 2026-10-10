- **WAL BENCH (#324, 2026-10-08)** The cargo feature `sim` of `buffer`, off by default
  (`buffer`'s dev-dependency on itself turns it on for the bench), adds
  `#[doc(hidden)] pub mod bench` with `Ring { new, commit }` over `wal::Writer`, as
  STORED BENCH does for `home`. Only the bench `benches/wal.rs` (`test = true`) uses it.
  `commit` appends its records, takes `trimmed`, syncs each record, and releases to the
  trimmed tail, so a time holds the writer's cost for each commit. The bench runs
  commits of 1 and 8 records, in rings of 64 and 4096 blocks: 1 record gives the cost
  for each commit (`trimmed`, `release`), 8 the cost for each record (`append`,
  `synced`). Lost: a copy of `wal.rs` in the bench through `#[path]`, which needs
  `entry`, `record`, and `crc32c` copied too and the workspace lints off; a time of
  `Buffer::append` and `committed` on a simulated file system, whose file writes hide
  the cost of the writer; and a control bench of code that a PR does not change, since
  the bench host gives A/A. It lands before the next PR after #1698 that changes
  `Writer::append`, `synced`, `release`, or `trimmed`. Decided by `laptop.architect`
  (2026-10-08T01:37:51Z):
  https://github.com/synnaxlabs/foundation/issues/324#issuecomment-6050388456. Its
  baseline is a quiet-host run of this bench at the head of #1729, not the #1698 rerun,
  whose scratch bench had no warm-up. The run makes two passes of one binary, and each
  median agrees within 3%. If one does not, a `crate:buffer` issue follows, and there is
  no baseline until it is fixed. Decided by `laptop.architect` (2026-10-08T03:12:46Z):
  https://github.com/synnaxlabs/foundation/pull/1729#issuecomment-6051403457.
  Supersedes "Its head numbers are the baseline of the `wal` benchmark" in
  https://github.com/synnaxlabs/foundation/pull/1698#issuecomment-6050306136. The
  #1698 numbers stay the record of the P1 judgment of #1698 only.
  `bench` also has `Logs { new, commit }` over `log::Logs`, for the bench
  `benches/logs.rs` (`test = true`). `commit` first hides the earlier records, as a
  trim that keeps up, then syncs one data entry of each index, so a time holds the
  cost of `Logs::sync` for each durable entry. Lost: a time of the commit on a
  simulated file system, whose file writes hide the sync. Lost: a time after a
  tagged entry in each log, as no sync of a data entry reads the tags. Decided by `laptop.architect` (#2236, 2026-10-10T05:43:41Z):
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094318889.
