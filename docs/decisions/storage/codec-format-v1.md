- **CODEC FORMAT V1 (#4)** A vector is a tag byte, a bit width byte, header fields,
  zeros to a multiple of the sample width, then a body padded the same way. Tags: 0 raw;
  1 FFOR (reference; body `sample - reference`); 2 delta (first, base; body `sample -
  previous - base` for each sample after the first); 3 RLE (`u16` run count; body the
  values, then `u16` lengths); 4 ALP (reference, `u8` exponent `e` and factor `f`, `u16`
  exception count; body `n - reference`, then the exception values, then their `u16`
  positions). Each field, sample, and `u16` is little-endian, and a vector holds at most
  1024 samples. Sample arithmetic is modulo 2^b, where b is the bit count of a sample.
  Packing is in natural order in every vector, least significant bit first. The person
  chose it ("Natural order") over FastLanes order for full vectors: a natural-order FFOR
  decode prototype took 197 ns per vector on an M3 Max, about 1.9% of a core at 100M
  samples/s. FastLanes order can come later as a new tag. Raw and RLE have bit width 0.
  Integers, `Stamp`, and `Span` use tags 0 to 3, `f32` and `f64` tags 0 to 4, and other
  scalars raw. A float vector of tag 1 to 3 holds the unsigned sortable integers of its
  samples, and its arithmetic and order are unsigned: for float bits `x` of b bits, the
  integer is `!x` when bit b - 1 of `x` is set, else `x ^ (1 << (b - 1))`. So -0.0 is
  `0x7FFF_FFFF` and +0.0 is `0x8000_0000` for `f32`. Delta over them is the fdelta of
  BQ4 (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/issues/2997#issuecomment-6230146181,
  2026-10-17T05:24:27Z). An ALP sample is the signed integer `n` in its float type `F`,
  times `P[f]`, times `N[e]`. The conversion and each product round to nearest, ties to
  even, with no fused multiply-add. `P[i]` and `N[i]` are the values of `F` nearest to
  10^i and 10^-i, each rounded once from the exact value: for `f32`, not the `f64` value
  rounded again. A float that no `n` gives (NaN, infinities, -0.0, and others) is an
  exception, held raw. `e` is at most 18 for `f64` and 10 for `f32`, `f` is at most `e`,
  and the exception positions strictly increase (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/issues/2997#issuecomment-6231723563,
  2026-10-17T08:26:13Z). How an encoder picks `e` and `f` is not part of the format. An
  `Encoder` keeps the pairs that its float series pick, so `home` keeps one encoder for
  each entry of a writer (#2997). Timestamp stride (BQ4) comes later as a new tag. The
  validator checks tags, bit widths, lengths, run sums, and the ALP exponents, factors,
  and positions, not padding. `max_len` of the raw length (raw plus one raw header per
  vector) sizes the output, and the encoder makes one pass. `codec/src/vector.rs` is the
  full spec. `codec` is the one place that checks a series against its count (#359):
  `encode` refuses raw values that do not hold `count` samples, and `validate` refuses
  encoded bytes that do not parse as `count` samples. The bytes do not carry the count,
  so a wrong count passes when the vectors also parse at it: a vector with bit width 0
  holds any count up to 1024. A fixed array is the series of its `count * len` elements.
  A `String`, `Bytes`, or `List` series is the `u32` series of its ends, then the series
  of its elements (`u8` for `String` and `Bytes`). An end counts elements from the
  first, so ends never decrease, and a `List` sample holds at most `max` elements. In
  the raw form, zeros pad the ends to a multiple of the element width or 8, whichever is
  less (R9-D3). A frame series starts on 8 bytes, so the elements are then aligned. The
  encoded form has no padding. `codec` owns the check of the ends, raw and encoded, and
  a view of a raw variable series relies on it. `encode`, `validate`, and `decode`
  refuse a `String` sample that is not UTF-8 (`Error::Utf8`, #556), and accept the same
  samples, so no reader checks UTF-8 again (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/issues/556#issuecomment-6055835549,
  2026-10-08T08:23:59Z). The check is one `std::str::from_utf8` pass and a read of each
  end. Not simdutf8 for now: it would be the first external runtime dependency of
  `codec`, and it runs unsafe SIMD code on input from peers. Trigger: a profile of a
  real or acceptance workload in which the UTF-8 check of `String` series takes more
  than 5% of the CPU of a node (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/pull/1845#issuecomment-6058900102,
  2026-10-08T11:31:12Z). Vector numbers in errors count across the ends and the
  elements.
  `decode` writes into a `&mut Vec<u8>` and sets its length to the raw length, so
  the caller does not compute the length of a variable series. It writes each byte
  of that length, padding included, and grows `out` only past its capacity, after a
  `validate` of the bytes (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/pull/2243#issuecomment-6094152516,
  2026-10-10T05:19:56Z), so bytes that do not hold `count` samples allocate nothing.
  Growth reads the bytes twice, so a caller reuses `out` (`laptop.architect`,
  https://github.com/synnaxlabs/foundation/pull/2243#issuecomment-6094291582,
  2026-10-10T05:39:48Z). `Variable` is how code outside `codec` writes and reads the raw
  form: `of` takes `String`, `Bytes`, and `List`, `len` gives the raw length of samples,
  `write` writes them, and `samples` reads them back. `write` checks neither UTF-8 nor
  `max`, because `encode` checks both; a `List` sample is its elements as little-endian
  bytes (#1826, `laptop.architect`,
  https://github.com/synnaxlabs/foundation/issues/1826#issuecomment-6093515181,
  2026-10-10T03:56:34Z).
  `Decoder` decodes a scalar series one vector at a time, so a reader of a series from
  a peer needs room for only 1024 samples, whatever the count (#416).
