- **DOCUMENT ENCODING (2026-10-04)** `document::encoding` gives each Document exactly
  one byte string, with no spans: a version byte, then tagged values, blocks in the
  producer's order, keys in byte order, and fixed-width little-endian integers (`u64`
  counts and lengths, `i128` integers, and `f64` floats as their bits). `decode`
  refuses every byte string that `Checked::encode` cannot write. Only a `Checked`
  Document encodes: `Checked::new` refuses nesting past 64 levels with `TooDeep`, so
  `Checked::encode` cannot fail, and `decode` gives a `Checked` or an `Error`. Front
  ends refuse files that nest deeper. `spec` holds a connector config as a `Checked`,
  and `config-hcl` `write` and `update` take one. Lost: a depth on each tree type,
  which makes each producer of a tree pay for a rule that only the writers (the
  encoding and `config-hcl`) need. `spec` stores and hashes these bytes. Pinned bytes
  are an oracle in `oracles/conformance/document/`. A new format takes a new version
  byte. Decided by the `config` builder; approved by the coordinator (#62). `Checked`
  decided by the architect (#828,
  https://github.com/synnaxlabs/foundation/issues/828#issuecomment-6030763787, and
  for `write` and `update`,
  https://github.com/synnaxlabs/foundation/issues/828#issuecomment-6030891911).
