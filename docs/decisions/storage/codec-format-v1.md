- **CODEC FORMAT V1 (#4)** A vector is a tag byte, a bit width byte, header fields,
  zeros to a multiple of the sample width, then a body padded the same way. Tags: 0
  raw; 1 FFOR (reference; body `sample - reference`); 2 delta (first, base; body
  `sample - previous - base` for each sample after the first); 3 RLE (`u16` run count;
  body the values, then `u16` lengths). Sample arithmetic is modulo 2^b, where b is
  the bit count of a sample. Packing is in natural order in every vector, least
  significant bit first. The person chose it ("Natural order") over FastLanes order
  for full vectors: a natural-order FFOR decode prototype took 197 ns per vector on an
  M3 Max, about 1.9% of a core at 100M samples/s. FastLanes order can come later as a
  new tag. Raw and RLE have bit width 0. Integers, `Stamp`, and `Span` use all four
  tags; other scalars use raw. Timestamp stride (BQ4) comes later as a new tag. The
  validator checks tags, bit widths, lengths, and run sums, not padding. `max_len` of
  the raw length (raw plus one raw header per vector) sizes the output, and the
  encoder makes one pass. `codec/src/vector.rs` is the full spec. `codec` is the one
  place that checks a series against its count (#359): `encode` refuses raw values
  that do not hold `count` samples, and `validate` refuses encoded bytes that do not
  parse as `count` samples. The bytes do not carry the count, so a wrong count passes
  when the vectors also parse at it: a vector with bit width 0 holds any count up to
  1024. A fixed array is the series of its `count * len` elements. A `String`,
  `Bytes`, or `List` series is the `u32` series of its ends, then the series of its
  elements (`u8` for `String` and `Bytes`). An end counts elements from the first, so
  ends never decrease, and a `List` sample holds at most `max` elements. In the raw
  form, zeros pad the ends to a multiple of the element width or 8, whichever is less
  (R9-D3). A frame series starts on 8 bytes, so the elements are then aligned. The
  encoded form has no padding. `codec` owns the check of the ends, raw and encoded,
  and a view of a raw variable series relies on it. `encode`, `validate`, and `decode`
  refuse a `String` sample that is not UTF-8 (`Error::Utf8`, #556), and accept the same
  samples, so no reader checks UTF-8 again (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/issues/556#issuecomment-6055835549,
  2026-10-08T08:23:59Z). The check is one `std::str::from_utf8` pass and a read of
  each end. Not simdutf8 for now: it would be the first external runtime dependency
  of `codec`, and it runs unsafe SIMD code on input from peers. Trigger: a profile of a
  real or acceptance workload in which the UTF-8 check of `String` series takes more
  than 5% of the CPU of a node (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/pull/1845#issuecomment-6058900102,
  2026-10-08T11:31:12Z). Vector numbers in errors count across the ends and the
  elements.
  `Decoder` decodes a scalar series one vector at a time, so a reader of a series from
  a peer needs room for only 1024 samples, whatever the count (#416).
