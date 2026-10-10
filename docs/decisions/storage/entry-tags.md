- **ENTRY TAGS (#191)** Each entry of an index log has a tag (S4) that says what its
  bytes hold: `DATA` 0 (STORED BODY), `HANDOFF` 1 (HANDOFF RECORD). A new kind of
  record takes the next free value here. The buffer gives only tag 0 a meaning:
  `Buffer::newest` takes a `NonZeroU8` and never gives an entry of tag 0. It compares
  each other tag with the tag its caller gives. That text supersedes "The buffer does
  not read the tag." (`laptop.architect`, #275,
  https://github.com/synnaxlabs/foundation/issues/275#issuecomment-6093580361,
  2026-10-10T04:04:50Z, and #2236,
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094205375,
  2026-10-10T05:27:21Z).
  Decided by the `write-path` builder; approved by the coordinator (#191).
  The buffer keeps, beside the logs, the offset of the newest record that holds a
  durable entry of each nonzero tag, only for a log that has one, so `Buffer::newest`
  reads one table for each record it gives. The offsets of a log go with the log: a
  change that drops a log drops them in the same place. Lost: one walk of the record
  tables from the newest record, 300 to 425 ms on a full ring of the node's layout
  (https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6093821909). Lost:
  the offsets of each tag in each log, +2.5 ns for each durable entry on the commit
  path and +93 B for each log
  (https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094187306); the
  offsets from the recovery walk only, which give a later call of `newest` a stale
  record. The offsets beside the logs cost +0.27 to +0.79 ns for each durable entry
  (https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094277124); P1
  accepts up to +0.8 ns. Decided by `laptop.architect` (#2236,
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6093837946,
  2026-10-10T04:37:55Z,
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094205375,
  2026-10-10T05:27:21Z, and
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094287815,
  2026-10-10T05:39:15Z). On box1, the tags take 84 B in all up to 3 pairs of a log
  and a nonzero tag, then 19.4 to 38.9 B for each pair, 2.2 MB at 100k logs with a
  handoff. A change that writes a second nonzero tag on a log states its bytes for
  each log and tag in its PR. Over 48 B, it needs a P1 judgment. Decided by
  `laptop.architect` (#2236,
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094432871,
  2026-10-10T05:58:44Z, and
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094641167,
  2026-10-10T06:26:36Z). The bytes replace those in item 3 of
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094432871.
