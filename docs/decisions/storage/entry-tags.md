- **ENTRY TAGS (#191)** Each entry of an index log has a tag (S4) that says what its
  bytes hold: `DATA` 0 (STORED BODY), `HANDOFF` 1 (HANDOFF RECORD), and the named
  records, 2 to 65: part `p` is tag `2 + p` (HOME NAMED READERS). A new kind of
  record takes the next free value here, 66. The buffer gives only tag 0 a meaning:
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
  each log and tag in its PR. Those bytes are a cost for each carried index, so P1 says
  when they need a judgment. At the count of P1, that is 5 B or more for each log: 0.5
  MB of idle memory at 100,000 logs. The 48 B bound of the rulings holds only for the
  `HANDOFF` tag, which they accepted with what it buys. Decided by `laptop.architect`
  (#2236, https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094432871,
  2026-10-10T05:58:44Z,
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094544644,
  2026-10-10T06:13:43Z, and
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094641167,
  2026-10-10T06:26:36Z). The bytes replace those in item 3 of
  https://github.com/synnaxlabs/foundation/pull/2236#issuecomment-6094432871. The
  trigger of a second tag supersedes "Over 48 B, it needs a P1 judgment" of those
  rulings (`laptop.architect`, 2026-10-12T16:12:06Z,
  https://github.com/synnaxlabs/foundation/pull/2423#issuecomment-6128077132, amended at
  2026-10-12T16:50:47Z,
  https://github.com/synnaxlabs/foundation/pull/2423#issuecomment-6128684558).
  The named records cost one pair for each part in use: an index of 7 records or
  fewer uses part 0 only, and each part past it costs 19.4 to 38.9 B for its log. The
  open calls `Buffer::newest` once for each tag up to the first that no index has an
  entry of: the most parts in use of an index, plus one. Decided by
  `laptop.architect` (2026-10-17T00:37:51Z:
  https://github.com/synnaxlabs/foundation/issues/2833#issuecomment-6227434486).
  The buffer gives each nonzero tag a meaning: it marks a state entry of at most
  `Layout::ENTRY_MAX_MIN` bytes (`Limit::Tagged`). Beside the logs, the buffer keeps
  a heap copy of the bytes of the newest durable entry of each nonzero tag on each
  path of an index, so `Buffer::newest` reads no file. The sync makes the copy from
  the parts of the record and reuses the bytes of the copy that it replaces; the
  open makes it with one read for each copy after the walk. On box2, at 100k pairs
  of a log and a tag, a pair takes the capacity of its largest copy since the open
  plus 53.7 B: 70.7 B for a handoff of 17 B, 126.7 B for a named part of 73 B,
  309.7 B for 256 B, and 4085.7 B for 4032 B, against 22.3 B before the copy. At
  100,000 logs with a handoff and a named part, that is 19.7 MB against 4.5 MB, and
  an open holds one copy of the largest tag more until it returns (10.4 MB for 73 B).
  `laptop.architect` accepted this cost under P1 for what it buys: a carry fills from
  memory with no file read and no new failure, and `newest` and `Shard::open` are
  sync with no error. It supersedes the 48 B bound of `HANDOFF` above, and the 84 B,
  the 19.4 to 38.9 B, and the 2.2 MB above. A change that adds to the 53.7 B states
  its bytes and needs a P1 judgment. Decided by `laptop.architect` (#3056,
  2026-10-17T18:45:34Z,
  https://github.com/synnaxlabs/foundation/pull/3056#issuecomment-6238846993). The
  person accepted the cost: "Yeah that's fine. I mean 100,000 tags is an insane
  amount on a raspberry PI, there's no need to be unreasonable" (#3056,
  2026-10-17T18:48:01Z,
  https://github.com/synnaxlabs/foundation/pull/3056#issuecomment-6238887273). This
  supersedes "The buffer gives only tag 0 a meaning" and the offsets beside the logs
  above. Decided by `laptop.architect` (#406, 2026-10-14T01:56:45Z,
  https://github.com/synnaxlabs/foundation/issues/406#issuecomment-6159164617).
