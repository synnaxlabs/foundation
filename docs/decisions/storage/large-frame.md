- **LARGE FRAME (#191)** The home refuses a write whose bodies no record of the ring
  holds, or with an entry whose parts, joined, pass `Buffer::block_max` of the shard's
  buffer, on either path, with `Large`. The home's header part counts in the entry's
  parts (#968). Decided, for the `Buffer::block_max` bound and the header part, by
  `laptop.architect` (2026-10-07T03:41:23Z):
  https://github.com/synnaxlabs/foundation/issues/1053#issuecomment-6030426386.
  Supersedes the bound of the largest block of the shard's pool,
  https://github.com/synnaxlabs/foundation/issues/968#issuecomment-6021211844. No seq
  moves and the home stores no part of the frame. The waiting handoffs of the frame's
  indexes are still recorded (HANDOFF RECORD). The writer splits the frame by samples or
  by indexes and writes each part. The home never splits a frame, because a frame
  applies whole (B7). Each handoff goes in its own append, so a handoff never makes a
  frame large. The size decides before the room, also when a handoff of the frame
  finds no room or the pool has no block for an index frame or a header:
  `Shard::write` then checks the lengths of the split against `Layout::check`,
  `Buffer::block_max`, and the largest block of the shard's pool, as `append` checks
  an entry with tag 0, and a frame whose draft no block of the shard's pool holds
  gives `Large` too. The check of the lengths of the split, which needs no block, is
  decided by `laptop.architect` (2026-10-20T11:45:27Z):
  https://github.com/synnaxlabs/foundation/pull/3578#issuecomment-6290103893. A backfill frame over a
  limit gets `Large`, not `Full`. The order is `Resend`, `Disk`, `Large`, then no room
  (`Lost` live, `Full` backfill). Decided by `laptop.architect` (2026-10-20T10:19:44Z):
  https://github.com/synnaxlabs/foundation/issues/3366#issuecomment-6288713154. It
  supersedes "The size is checked only when the bodies are appended, after the
  handoffs: a frame whose handoff finds no room is lost (live) or refused with `Full`
  (backfill) before its size is known" (#191). Lost: a `buffer` function that checks
  a batch as `append` does, for one caller. Trigger to look again: a new `Limit` that
  an entry with tag 0 can meet. After
  a failed commit, the home gives `Disk` before it checks the size, so a frame that no
  record of the ring holds, or with an entry whose parts, joined, pass
  `Buffer::block_max`, gets `Disk`, not `Large`, on either path (#1260). Decided by
  `laptop.architect` (2026-10-07T09:21:07Z):
  https://github.com/synnaxlabs/foundation/issues/1260#issuecomment-6034929252. The
  sentence departs from that ruling on the bound, approved by `laptop.architect`
  (2026-10-12T20:04:12Z):
  https://github.com/synnaxlabs/foundation/pull/2464#issuecomment-6131705729.
  `Shard::write_body_max(set)` gives body bytes of a frame of `set`, raw or encoded,
  that `Shard::write` never refuses with `Large` and that an idle shard applies, as a
  lower bound of the most such bytes: the least of the largest block of the pool less the frame overhead; for
  each group, of `Buffer::block_max` less the stored header and of the largest block
  less the overhead of its frame; of one record for the entries of all groups; and of
  `Pool::budget` less the footprints, by `block::footprint`, of the blocks that the
  pool holds at once: the draft of an encoded frame of more than one group, the index
  frame and header of each group, and the newest frame of each index, which the shard
  keeps at rest. Each term goes through `codec::len_within`. The batch term is
  decided by `laptop.architect` (2026-10-20T11:30:31Z):
  https://github.com/synnaxlabs/foundation/pull/3578#issuecomment-6289857046. The
  pool term is decided by `laptop.architect` (2026-10-20T11:45:27Z):
  https://github.com/synnaxlabs/foundation/pull/3578#issuecomment-6290103893. Lost: a
  `buffer::Layout` function that gives the room of a batch of entries. Trigger to look
  again: a second caller, or a change of the entry table.
  `Shard::read_body_max` gives the longest body of one stored entry. `home` owns the
  entry layout, so it computes both (#3366). Decided by `laptop.architect`
  (2026-10-20T10:07:34Z):
  https://github.com/synnaxlabs/foundation/issues/3366#issuecomment-6288511691.
