# Parameters for experiment


- Delivery and wire: group commit interval, credit window (bytes, from link BDP), batch
  size, linger, max packet size, latest-over-TCP send buffer, priority mapping, catch-up
  merge size.
- Storage: write-ahead ring size, ring record alignment, segment flush size and age,
  chunk sizes, memtable cost per channel (estimate 100 to 200 bytes), eviction timing.
- Codecs: ALP refresh interval and skip rule (R10-D8), natural-order delta decode speed
  on a Pi 4 (R10-D5), fdelta on recorded plant data (R10-D4), when to build `max`
  (R10-D7), short-vector packing.
- Memory: allocator (mimalloc 3 off the hot path), pool size classes and budget, commit
  and purge policy, queue kinds and capacities, spin windows (0 on a Pi), latest
  mailbox mechanics, a compact copy of the current value (B4), cache-line padding.
- Replication: seq block size, node lease length, check-in period, fence margin, gate
  grace, standby send point (after sync or on receipt), SSD rule for Pi homes.
- Consensus and spec: Raft timeouts, prolly chunk size (~4 KiB) and chunker quality,
  root GC depth (last N roots).
- Time: exchange period, source discovery period, drift rate for bound widening
  (starts at 200 ppm, ESTIMATE COMBINE), stamp limits near 1970 and far future (A5).
- Transport: default carrier per traffic class (QUIC vs TLS over TCP, measured on
  Linux), GSO and GRO, ChaCha20 vs AES by platform, relay selection, the retry
  interval of a read that waits for a block (RECV WAITS), the `Complete` share of the
  turn (3 to 1, `LATEST_COST`).
- Compression and reduction defaults; retention defaults; disk budget defaults.
- Benchmark reruns owed: r1 handoff, r10 codecs, r11 memory on Linux x86-64 (pinned)
  and Raspberry Pi 4; `sim` binary size against P1 (BQ19); binary size and idle memory
  of R7 dependencies.
