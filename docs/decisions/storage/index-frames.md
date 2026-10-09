- **INDEX FRAMES (#191)** The home makes one index frame for each present group with
  samples of a write: the writer's key set with only that group present, its range,
  and its encoded series. The home stores it, keeps it as the index's newest frame,
  and later gives it to readers. B7, the log, the seq, and reader positions are per
  index. A write with more than one present group pays one copy of its series into
  the index frames. Decided by the `write-path` builder; approved by the coordinator
  (#191). A group with no samples gets no index frame and no data entry. It still
  records its handoff (or keeps it waiting when it finds no room), renews its lease,
  spends a seq range of zero, and is applied, also when another group of a live write
  is lost or its own handoff finds no room in a live write. A backfill write with no
  room gets `Full` whole (B5). A write with no samples still reports a failed commit.
  Decided by the coordinator with the advisor at 2026-10-06T15:30:26Z (#885):
  https://github.com/synnaxlabs/foundation/issues/885#issuecomment-6019665440
  A group with no samples is confirmed with the entries appended before it. It appends
  no entry of its own and moves no stored mark, but a handoff that it records does. A
  lost range is durable only when a later live entry of its index, with samples or a
  handoff, is on disk. A restart before that continues the index at the lost range's
  first seq. Supersedes the confirm rule of
  https://github.com/synnaxlabs/foundation/issues/885#issuecomment-6019665440.
  Decided by `laptop.architect` (#1347), with the live write and `Full` text above,
  in three comments (2026-10-07T11:46:56Z, 11:47:17Z, and 11:52:25Z, in this order):
  https://github.com/synnaxlabs/foundation/pull/1347#issuecomment-6037240549
  https://github.com/synnaxlabs/foundation/pull/1347#issuecomment-6037245942
  https://github.com/synnaxlabs/foundation/pull/1347#issuecomment-6037325066
