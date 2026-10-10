- **DOCUMENT MODEL (2026-10-04)** A Document is attributes in a map sorted by key
  (keys unique) plus blocks in order. Values: bool, integer (`i128`), finite float,
  string, reference (`types::name::Name`), list, map, and call. No null and no
  expressions. Refines K1: a front end gives every key, keyword, label, function
  name, and value a span (byte offset, then line and column in Unicode scalar values,
  from 0); SDK and spec documents have none (`docs/decisions/where/definitions.md`, kind
  config). `==` never reads spans, so a Document from a file equals the same Document
  from the spec. Decided by the `config` builder; approved by the coordinator and
  `consensus` (#42).
  Amended (2026-10-10, #1914): `Position` and `Span` derive `PartialOrd` and `Ord`: a
  position by offset, then by line and column, and a span by source, then by start,
  then by end. This agrees with `Eq`. Decided by `laptop.architect-2`
  (2026-10-10T02:45:50Z):
  https://github.com/synnaxlabs/foundation/issues/1914#issuecomment-6092943513.
