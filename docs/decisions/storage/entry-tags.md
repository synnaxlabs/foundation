- **ENTRY TAGS (#191)** Each entry of an index log has a tag (S4) that says what its
  bytes hold: `DATA` 0 (STORED BODY), `HANDOFF` 1 (HANDOFF RECORD). A new kind of
  record takes the next free value here. The buffer gives a tag no meaning: only
  `Buffer::newest` compares it, with the tag its caller gives. That sentence supersedes
  "The buffer does not read the tag." (`laptop.architect`, #275,
  https://github.com/synnaxlabs/foundation/issues/275#issuecomment-6093580361,
  2026-10-10T04:04:50Z).
  Decided by the `write-path` builder; approved by the coordinator (#191).
  Each index log keeps the offset of the newest record that holds a durable entry of
  each tag, so `Buffer::newest` reads one table for each record it gives. Lost: one
  walk of the record tables from the newest record, 300 to 425 ms on a full ring of
  the node's layout
  (https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6093821909).
  Decided by `laptop.architect` (#2236,
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6093837946,
  2026-10-10T04:37:55Z).
