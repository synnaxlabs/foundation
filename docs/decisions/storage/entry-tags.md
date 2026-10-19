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
  the open peaks at 80.19 MB against 40.02 MB before the copy (box2, sim files,
  100,000 indexes with an entry of 17 B and one of 73 B): the walk's map of tagged
  entries and its sorted `Vec` hold 24.9 MB above the kept state until the open
  returns. `laptop.architect` accepted the peak for 406-1 (#3056,
  2026-10-17T21:24:52Z,
  https://github.com/synnaxlabs/foundation/pull/3056#issuecomment-6241330619).
  `laptop.architect` accepted this cost under P1 for what it buys: a carry fills from
  memory with no file read and no new failure, and `newest` and `Shard::open` are
  sync with no error. It supersedes the 48 B bound of `HANDOFF` above, and the 84 B,
  the 19.4 to 38.9 B, and the 2.2 MB above. A change that adds to the 53.7 B states
  its bytes and needs a P1 judgment. The `via` node of a handoff that names a holder
  adds 16 B to its copy, up to 64 B for a subject of 240 to 255 B: 1.6 MB at 100,000
  logs (`laptop.architect`, #3379,
  https://github.com/synnaxlabs/foundation/pull/3379#issuecomment-6270682141).
  Decided by `laptop.architect` (#3056,
  2026-10-17T18:45:34Z,
  https://github.com/synnaxlabs/foundation/pull/3056#issuecomment-6238846993). The
  person accepted the cost: "Yeah that's fine. I mean 100,000 tags is an insane
  amount on a raspberry PI, there's no need to be unreasonable" (#3056,
  2026-10-17T18:48:01Z,
  https://github.com/synnaxlabs/foundation/pull/3056#issuecomment-6238887273). This
  supersedes "The buffer gives only tag 0 a meaning" and the offsets beside the logs
  above. Decided by `laptop.architect` (#406, 2026-10-14T01:56:45Z,
  https://github.com/synnaxlabs/foundation/issues/406#issuecomment-6159164617).
  Amended (2026-10-18T01:33:44Z, #406, 406-2): the holders of each copy, so a trim
  never passes the record of a copy, add to each pair. A copy takes 72 B beside its
  bytes. On box2, with sim files and 100,000 indexes with an entry of 17 B and one of
  73 B, the buffer holds 68.42 MB against 40.02 MB before the copy and 55.31 MB after
  406-1: +28.40 MB, about 142 B a pair above the cost before the copy. The open peaks
  at 78.86 MB against 51.62 MB before the copy and 80.19 MB after 406-1. At 10,000
  indexes, it holds 7.96 MB against 4.76 MB. The person accepted about 31 MB held at
  100,000 indexes: "Ok this is fine" (#406, 2026-10-18T00:38:09Z,
  https://github.com/synnaxlabs/foundation/issues/406#issuecomment-6243808602).
  `laptop.architect` accepted the held bytes and the peak under P1 (#406,
  2026-10-18T01:33:44Z,
  https://github.com/synnaxlabs/foundation/issues/406#issuecomment-6244272935). This
  supersedes the 53.7 B above: a change that adds to the bytes of a pair states them
  and needs a P1 judgment.
  Amended (2026-10-18T06:40:21Z, #406, 406-2): a tagged entry may cost up to +13 ns
  over the code before 406-2 on the quiet-host `deadline_tagged/32` line (+10.2 ns at
  `c025fb18b` against `4b019d5e8`). HOME NAMED READERS holds up to 64 parts an index,
  and a tick appends each part whose records changed: at 100,000 indexes, that is
  1.26 ms/s with one part and about 81 ms/s with 64. The rate follows the ticks and
  the sessions, not the data. It buys whole records at each carry, so no named position
  is lost at a carry or an open. `laptop.architect` accepted it under P1 (#3113,
  2026-10-18T06:40:21Z,
  https://github.com/synnaxlabs/foundation/pull/3113#issuecomment-6246659111, item 2).
  A cost over +13 ns a tagged entry against the code before 406-2 needs a new P1
  judgment.
