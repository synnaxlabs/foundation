- **R4 SETTLED** Own sans-I/O Raft modeled on etcd/raft, PreVote and CheckQuorum on,
  with etcd scenarios and the TLA+ spec as oracles. No gossip. Each region's spec is one
  prolly tree keyed by full name, about 4 KiB chunks, BLAKE3. Each change record lists
  its new chunks. A region's voters sit on one LAN. A node fetches only the regions and
  ranges it uses.
