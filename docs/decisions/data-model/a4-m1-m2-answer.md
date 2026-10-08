- **A4 + M1/M2 answer** `channel::Key` is a UUIDv7 made with the channel. It is never
  reused and never changes. Files carry names only. The stored spec maps name to key,
  and `apply` assigns a key the first time a name appears. Renames are explicit
  (`foundation rename`), and `plan` shows them. UUIDs appear only in the stored spec,
  wire setup, and disk footers.
