# Per-index state at the home

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Encoded samples | Index log (write-ahead ring, then segments), as stored bodies (STORED BODY) | `home` and `replica` through `buffer.append` | Complete readers (catch-up), `replica`, crash recovery | `buffer`, `home` (stored body) |
| Seq counters (live, backfill) | Memory at the home; durable through the index log | `home` | `delivery`, `wire` (prediction) | `home` |
| Control state | Memory in `control` at the home; handoff records in the index log (HANDOFF RECORD; truth, copied by `replica`); control channel (published copy) | `control` decides, `home` records | New home at takeover (from the log, X18) | `control`, `home` |
| Control lease | A writer session setting; state in `control` | The writer at open | `control` | `control` |
| Reader positions | Truth: `delivery` state at the home, written as index log records and copied by `replica`. A connected reader's `hub` keeps its own position. Status channels publish copies | `delivery`; `replica` copies; `node` publishes | `home` after failover; `hub` on resume | `delivery`, `buffer`, `replica` |
| Holds and floors | `delivery` (hold per reader and index); floor = lowest held position per path, handed by `home` to `buffer.set_floor` with the retention cutoff | `delivery` | `home`, `buffer` | `delivery`, `buffer` |
| Backfill dedup marks | Index log records | `home` | `replica`, a new home | `home` |
| Gaps | Index log records (explicit gap with a count) | `home`, `buffer` | Complete readers | `home`, `buffer` |
| Stored and replicated marks | Memory at the home (the replicated mark is the standby's position in `delivery`); published on status channels | `home`, `delivery` | Writers (confirmation), `node` collector | `home`, `delivery` |
| Latest mailbox | Memory: depth 1 per latest reader per index | `delivery` | The reader session | `delivery` |
| Current value | Memory: the index's newest live frame, one pinned pool block per index (B4, MEMORY BOUNDS) | `delivery` | A new latest reader | `delivery` |
| Credits | Memory per session per index; credit messages on the wire | The reader's `hub` grants; `delivery` spends when the home releases a frame | `delivery` | `delivery`, `wire` |
| Live frames for complete readers | Memory: the index's live frames not yet on disk, and the frames released to each complete session and not taken, as refcount clones (B1, CREDIT RULES, MEMORY BOUNDS) | `delivery`: the home queues each stored live frame and releases them after a commit | The reader session | `delivery` |
| Masks and routes | Memory: mask per key set and reader; route per key set | `delivery` | The home's fan-out | `types` (mask), `delivery` |
| Death records | Quality channel samples (X19) | `home` | Sinks | `home` |
| Read copy data | The copy node's index log | `replica` | The copy's readers, served by `home` in copy mode (X43) | `replica`, `home` |
