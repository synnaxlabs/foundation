- **LARGE FRAME (#191)** The home refuses a write whose bodies no record of the ring or
  no block of the shard's pool holds, on either path, with `Large`. The pool bound is on
  each entry's parts joined, the home's header part included (#968). No seq moves and
  the home stores no part of the frame. The waiting handoffs of the frame's indexes are
  still recorded (HANDOFF RECORD). The writer splits the frame by samples or by indexes
  and writes each part. The home never splits a frame, because a frame applies whole
  (B7). Each handoff goes in its own append, so a handoff never makes a frame large. The
  size is checked only when the bodies are appended, after the handoffs: a frame whose
  handoff finds no room is lost (live) or refused with `Full` (backfill) before its size
  is known. Decided by the `write-path` builder (#191). After a failed commit, the home
  gives `Disk` before it checks the size, so a frame that no record of the ring or no
  block of the pool holds gets `Disk`, not `Large`, on either path (#1260). Decided by
  `laptop.architect` (2026-10-07T09:21:07Z):
  https://github.com/synnaxlabs/foundation/issues/1260#issuecomment-6034929252.
