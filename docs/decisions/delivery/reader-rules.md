- **READER RULES (write-path and advisor, 2026-10-04)** A position is one cumulative seq
  per path: the first sample the reader has not received. A reader that does not record
  has no backfill position. An open session holds all data it has not received. A closed
  named reader holds from its position until `hold` after the close, in mesh time; an
  unnamed reader holds nothing after it closes. A hold is zero or more; `config` rejects
  a negative hold (#94). The floor per path is the lowest held position, or none.
  Retention caps the holds: the floor of a path is at least the lowest seq whose store
  time is at or after the cutoff (the mesh time of `home` minus `keep`), or past its
  last sample when no seq is (RETENTION). The cutoff moves with time, so `home` gives it
  on its interval, not only when a position moves. After a failover the store times of a
  path need not rise with its seq, so a sample stored before the cutoff can stay held a
  little longer, and no sample stored at or after the cutoff loses its hold (decided by
  `laptop.architect`, 2026-10-07T13:39:39Z:
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6039184732). Before
  its first estimate `home` has no mesh time and gives no cutoff, so the cap starts at
  the first estimate (decided by `laptop.architect`, 2026-10-07T16:35:57Z:
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6042343145). These
  supersede, in their clauses that `home` gives `set_floor` the index's `keep`, that
  `buffer` raises the floor past each sample stored more than `keep` ago, and that the
  "Holds and floors" row of `docs/decisions/where/home-state.md` stays, part 2 of
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6037946637,
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6037950577, and the
  floor sentence of
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6038431739. Part 3
  of 6037946637 (at `0s` the floor is the stored mark) holds only after the first
  estimate, and only when each store time of the path is before the mesh time of `home`.
  After a failover, a copied store time can be after the mesh time of the new home. A
  trim follows STORE TRIM: under disk pressure, at the tail of the ring, whatever the
  floors (decided by `laptop.architect`, 2026-10-07T12:59:37Z:
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6038431739).
  Supersedes https://github.com/synnaxlabs/foundation/pull/89 in its clause that
  `buffer` trims below the floor, past retention (by store time), and under disk
  pressure. A resume takes, per path, the position the reader's `hub` presents, then the
  position at this home, then the home's fallback. A position below the floor or past
  the head is accepted as is; the `buffer` read reports any gap (B2). A resume starts a
  `buffer` read at `Mark::at(position)`, so the entries with no samples at the position
  come again. Between reads, the caller keeps the mark the last read gave, in memory
  (#510). Named readers write a position record at once when they open, close, or are
  taken over, and on the home's interval when the position changed. A session open at a
  crash restores as closed at the restore. The home drops a grant, ack, or close for a
  key it gave that is no longer open: a late message after a close or a takeover. A take
  of such a key gives nothing. After a home sheds an index (HOME SURFACE), a call with a
  key of a reader of the index panics until the home carries it again; after that carry,
  the home drops the call as late (`laptop.architect`, 2026-10-08T22:24:43Z:
  https://github.com/synnaxlabs/foundation/issues/2020#issuecomment-6070259814). A key
  is the home's own value, in memory only, and keys start again at a restore. No hub
  message carries a key: the home maps each one to a key it gave, so a key it never gave
  is a defect of the home and panics. Complete and latest sessions have separate key
  types, so a call in the wrong mode does not compile (advisor, #725; the take and the
  key rule: architect, #1038). Only a named complete session needs mesh time to close.
  One `Readers::close(key, now)` ends each session: `now` is `None` before the home
  first has mesh time, and a close with `None` of an open named complete session panics.
  `Readers::open_named_latest` takes a stamp, and no other open does, so the home opens
  unnamed readers before the first estimate. A named complete session has a
  `complete::Key` (#1024). One close replaces `Readers::close_named`, so the caller
  never picks a close by the mode of the session (`laptop.architect`,
  2026-10-08T11:12:45Z:
  https://github.com/synnaxlabs/foundation/pull/1863#issuecomment-6058607367). One
  `delivery::named::Key { subject, name }` keys a named reader in `Reader::Named`,
  `Record`, and `Readers::open_named_latest`, in place of two `Name` values (the
  director's question, 2026-10-08T11:03:04Z:
  https://github.com/synnaxlabs/foundation/pull/1856#issuecomment-6058445903;
  `laptop.architect`, the same #1863 comment). Supersedes the B3 single position, the
  `close` and `close_named` pair and its wrong-close panics of
  https://github.com/synnaxlabs/foundation/issues/1024#issuecomment-6029883335, and
  https://github.com/synnaxlabs/foundation/issues/1024#issuecomment-6030162160. Basis:
  A6, A8, B2, B3, S10, X14, #41.
