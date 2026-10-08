- **BQ7** Live writes never wait. The home publishes stored and replicated marks per
  index. Each writer picks its confirmation and resends unconfirmed frames after
  failover, deduplicated. Voters promote the standby when the home's lease lapses,
  however far behind it is. The old home's tail returns as deduplicated backfill.
  Failback is manual. By default the home sends to the standby after its own sync (an
  SSD is advised for critical homes; a Pi SD card loses about 1.6 s per crash).
