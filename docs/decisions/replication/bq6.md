- **BQ6** Asynchronous replication. The `replica` component ships each index's log
  (stored bytes, reader positions, control handoffs, dedup marks) without touching the
  write path. Takeover is the home's crash recovery plus one fence check, inside `home`.
  Supersedes: r8 Q6 standby as a reader.
