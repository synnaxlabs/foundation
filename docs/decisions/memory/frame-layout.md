- **FRAME LAYOUT (refines M3, X8)** `types::frame`, little-endian, offsets from the
  start of the payload. A 16-byte header: key set key, range count, and descriptor count
  (each u32), then form and path (each u8), then zeros. The path is live 0 or backfill
  1, the path whose seq the ranges count on (A8).
  Then `{ group: u32, count: u32, seq: u64 }` for each present group, sorted by group,
  then `{ entry: u32, end: u32 }` for each present series, sorted by entry, then the
  series. `end` counts from the start of the series bytes. Each series starts at the
  end before it rounded up to 8, and the first at 0. A group is present when its index
  is, and a present series needs its index. A lookup by entry or group is a binary
  search, and a pass in entry order reads each descriptor once. The header holds no
  entry or group count: an entry or group past the key set is absent. A frame is at
  most `u32::MAX` bytes. The series bytes are stored and sent as they are (X35), so
  their order and padding are part of the disk and wire format version (C9d). A change
  to either needs a new version. `Layout::draft` writes zeros in the padding of a frame
  from lengths. A frame from ends gets its padding from `Draft::body_mut`, which the
  caller fills whole (decided by the architect, #1246, 2026-10-07T15:17:43Z:
  https://github.com/synnaxlabs/foundation/issues/1246#issuecomment-6040879844).
  Supersedes the zero padding of every draft in
  https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6031091642. No
  reader reads the padding, so `frame::check` does not check it. A frame from a peer may
  hold other bytes there, which `replica` stores and copy mode (X43) sends as they are
  (decided by the architect, #1064:
  https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6031091642, worded in
  https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6031226370). The
  padding is at most 7 bytes for each present series: at most 1% of encoded bytes at
  1024 samples, and up to 34% at 10 samples (measured on #317). `frame::split` cuts a
  body at its `(tag, end)` pairs and panics on ends that do not fit. Copy mode runs
  `frame::check` once where remote records enter (X43). Decided by the coordinator
  (#306). A frame from another node is built from its ends (HUB WIRE):
  `Layout::from_ends` checks them before a block is taken, and `Draft::body_mut` takes
  the body as it arrives. `frame::ends` gives the ends of series of given lengths, so
  the rule of 8 has one home. Both nodes charge such a frame with
  `frame::charge(series, body_len)`, one function on each side, so the charges are
  equal by construction; a `Layout::charge` would be a second way. Decided by the
  architect (#1068:
  https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6031655359 and
  https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6032304827).
