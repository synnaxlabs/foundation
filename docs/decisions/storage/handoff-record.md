- **HANDOFF RECORD (#191)** The home records each handoff that `Gate::handoff` gives
  (GATE RULES) as a buffer entry on the live path of the index, with tag `HANDOFF`,
  `len` 0, and `first` at the live tail. It records a handoff after the gate input that
  gave it and before the next input or frame. Its bytes are empty when no writer holds
  control, else `[authority: u8]` then the holder's subject as UTF-8; the entry length
  gives the subject's length. A restart or a failover starts the gate from the last
  record (X18): `Gate::recover` with its holder, or `Gate::new` when it names none.
  Trimming must keep the last record of each index (#406). Until it does, a trim
  (STORE TRIM) can free that record, and a holder whose record a trim freed gets no
  grace after a restart. Retention deletes nothing (decided by `laptop.architect`,
  2026-10-07T12:30:53Z and 2026-10-07T12:59:37Z:
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6037946637 and
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6038431739).
  Supersedes https://github.com/synnaxlabs/foundation/pull/402 in its clause that
  retention can remove that record. The layout is part of the disk format version (C9d).
  Copy mode checks each record once where remote records enter (X43), and the read after
  it panics on a bad record. Decided by the `write-path` builder; approved by the
  coordinator (#191).
