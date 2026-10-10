- **R9 type decisions (SETTLED BY ME)** R9-D1 per-entry types are interned once in the
  key set; R9-D2 bools are one byte; R9-D3 raw series are padded to element width and
  blocks are 64-byte aligned; R9-D4 variable-length series are `ends[n]` then data;
  R9-D5 `types` is byte layout only and meaning lives in the spec; R9-D6 `time::Span`
  value and `duration` keyword; R9-D7 JSON uses RFC 3339 UTC with 9 fraction digits,
  span unit strings, and keys as UUID strings, and never appears on the data path; R9-D8
  exact reduced-fraction `Rate` with u128 offset math; R9-D10 panic on internal overflow
  and checked math for outside values; R9-D13 `types` modules are time, sample, series,
  frame, channel, node, quality, name, hash (R16-7), authority (#57: `access`, `spec`,
  `wire`, and `control` all use it), and digest (the BLAKE3 address of spec chunks and
  blobs, which `spec`, `blob`, `wire`, and `mesh` share); R9-D14 checks run once, at the
  home. R9-D11 rejected (slots won). Supersedes: R9-D13 `block` module (by SRP PASS).
