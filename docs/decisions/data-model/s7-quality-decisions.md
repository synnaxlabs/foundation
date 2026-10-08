- **S7 + QUALITY DECISIONS** A struct is a template of channels: one channel per field,
  all on one index. The runtime knows only primitive, fixed array, list, enum, flags,
  string, and bytes channels. Optional field presence is per frame (a presence mask over
  the writer's key set). Struct views are built from one frame, never from per-field
  latest values. Disk chunks record the seq ranges they cover. Supersedes: A10, A14
  validity bits, A15 struct fingerprints, S2 struct layout.
