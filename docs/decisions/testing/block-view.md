- **BLOCK VIEW (#110)** `Block::skip(self, count)` is a view of the same buffer that
  starts `count` bytes later, with no copy and no count change. `Block` is
  `{ header, start: u32, len: u32 }`, 16 bytes, so the largest block holds 2 GiB; a
  budget above that gives more blocks, not larger ones. A pool has at most 96 size
  classes, four per doubling, so a payload is at most 64 bytes or a quarter above its
  length (#188; 26 power-of-two classes wasted up to 100%). `slice(&self, range)`
  lost: it clones the count for every view, and nothing needs a range yet. Decided
  by `memory`.
  Amended (2026-10-07, #1068): `block::footprint(len)` gives `usize::MAX` when `len`
  passes the largest payload, in place of a panic. No pool holds such a block, so
  every budget refuses it. `frame::charge` of ends from a hostile peer gives
  `u64::MAX`, and `Layout::draft` refuses it with `block::Error::TooLarge { .. }`
  (`frame::Error::Pool` at the reader) and takes no block. A reader drafts before it
  spends, so a spend adds only a charge that a pool holds, and a plain add never
  overflows. Lost: an exported largest payload with a new `Error` variant, a second
  check of a limit that `block` owns; a saturating spend, a second guard.
  Decided by the architect, #1068
  (https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6032386156,
  corrected in
  https://github.com/synnaxlabs/foundation/pull/1216#issuecomment-6032799083 and
  https://github.com/synnaxlabs/foundation/pull/1504#issuecomment-6043266054, the
  latter at 2026-10-07T17:32:11Z).
  Amended (2026-10-07, #1504): a const assertion in `block` checks the 16 bytes of a
  handle, so `block`, and each crate that depends on it, builds only where a pointer is
  8 bytes. So `frame::charge` maps no `usize::MAX` of a narrower target to `u64::MAX`,
  and no crate that depends on `block` checks the pointer width (`usize::BITS` or
  `target_pointer_width`); #1396 removes the last such check, in `os`. A 32-bit target
  first needs a new handle, and the choice of targets is the person's (CPU BASELINE);
  `charge_of` in `types` changes with that handle. Decided by `laptop.architect`
  (2026-10-07T18:30:48Z):
  https://github.com/synnaxlabs/foundation/pull/1504#issuecomment-6044276677
