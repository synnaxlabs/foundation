- **R5 starting points** Addresses come from the mesh, not DNS. A TCP path is
  mandatory. Try direct UDP, then direct TCP, then a relay. Relays admit only known
  keys. No n0 infrastructure. iroh `Endpoint` is rejected; quinn-proto is the fallback
  core. The diode carrier is UDP, Noise K, RaptorQ, seq, and codec keyframes: best
  effort with recorded gaps; commands, Raft, and clock exchange cannot cross it.
