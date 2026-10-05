# Foundation r10: adaptive and yield-gated compression

Research and a local benchmark for the user's request: "Random sensor data can be
complex and sometimes not worth compression. Maybe even setting compression policies.
Not worth costing CPU time if we don't get compression yield."

## Summary

- **The light codecs make the gate cheap and exact.** One stats pass per 1024-value
  vector gives the exact encoded size of every light codec: FFOR, delta, RLE, stride,
  and float delta. On the Apple M3 Max, the pass costs 0.10-0.20 ns/value for 8-32-bit
  types and 0.45-0.48 ns/value for 64-bit types. The selector does not guess, so it
  needs no sampling and no hysteresis. Only ALP's exponent choice needs sampling.
- **Yield collapses less often than expected.** ADC containers are wider than the
  signal's noise, so noisy counts still compress: 16-bit counts with 256 LSB of noise
  give 1.36x, and 1 LSB of noise gives 3.96x. Only full-range white noise gives 1.00x.
  Calibrated `f64` values are the weak case at 1.61-1.80x. They cost 4.4-5.0
  bytes/sample, which is over P1's 4-byte budget.
- **General-purpose codecs are the CPU waste.** zstd -1 gets 0.99-1.33x on noisy numeric
  data at a median of 7.9 ns/value. That is about 30x the light selector on integers.
  pco makes noisy data 2-23% smaller than the light codecs do, but it costs 12-118
  ns/value to encode.
- **Recommendation.** Select per vector from exact sizes, with raw always a candidate.
  Use a fixed minimum saving of 1/8. Put a 1-byte codec tag in every vector. Add float
  delta (`fdelta`) and drop ALP_rd. Use a `[[compression]]` policy whose `mode` is
  `auto`, `raw`, or `max`. Channels need no configuration to get `auto`.

## 1. Prior art

| System | Decision unit | How it decides | Yield threshold | State across blocks | Source |
|---|---|---|---|---|---|
| ZFS | Record (128 KiB default) | Compresses, then keeps the result only if it is small enough | Saves at least 12.5% (`s_len - (s_len >> 3)`, "legacy value"), then rounds down to the allocation size | None | `module/zfs/zio.c` `zio_get_compression_max_size` |
| ZFS zstd early abort | Record of 128 KiB or more, zstd level 3 or higher | Probe with LZ4. If LZ4 fails, probe with zstd-1. If that also fails, store raw | Same 12.5% | None | `module/zstd/zfs_zstd.c`. A comment says LZ4 alone lost up to 8.5% of savings on very compressible data |
| Btrfs | 128 KiB range | Byte heuristic on 16 bytes of every 256: repeated patterns, then byte-set size, then core set (≤64 compress, ≥200 skip), then Shannon entropy (≤65% compress, <80% compress, else skip) | Entropy 80% | **Sticky**: one failed compression sets `BTRFS_INODE_NOCOMPRESS` on the file, unless `compress-force` is set | `fs/btrfs/compression.c` `btrfs_compress_heuristic`, `fs/btrfs/inode.c` |
| RocksDB | Data block (about 4 KiB) | Compresses each block, then compares the result to a byte limit | `max_compressed_bytes_per_kb = 1024*7/8` (12.5%) | Experimental `auto_skip`: a running ratio estimate stops attempts when compression does not pay. It still tries a sampled fraction of blocks so that it can resume. Off by default | `include/rocksdb/compression_type.h` (`CompressionOptions`) |
| Cassandra | Chunk (16 KiB default) | Stores a chunk raw if it compresses past `maxCompressedLength` | `min_compress_ratio`. The default is 0.0, which turns the gate off (single source) | None | `CompressionParams.java` |
| VictoriaMetrics | Block of one series | Picks a `MarshalType` from the values (const, delta-const, gauge, or counter), then runs zstd | Blocks under 128 bytes are not compressed. A zstd output larger than 90% of the input is discarded and the block is stored plain | None | `lib/encoding/encoding.go` |
| Arrow IPC | Buffer | Writer option (`min_space_savings` in Arrow C++, single source) | Uncompressed length `-1` means "stored raw", for "cases where compression does not yield appreciable savings" | None | `format/Message.fbs` `BodyCompression` |
| Parquet | Data page | v2 page header has `is_compressed`. parquet-java chooses dictionary or plain encoding on the **first page only** (`encoded + dictionary < raw`) and later falls back only when the dictionary grows too large | Dictionary must beat plain | Decision is fixed after the first page | `parquet.thrift`, `FallbackValuesWriter.java`, `DictionaryValuesWriter.java` |
| QuestDB | Native column files are uncompressed. The docs recommend ZFS with LZ4. Parquet pages use `lz4_raw` | Keeps the compressed page only if the ratio reaches `cairo.partition.encoder.parquet.min.compression.ratio` | 1.2 (about 17%) | None | QuestDB docs (single source) |
| TimescaleDB | Column segment (up to 1000 values) | Fixed algorithm per type: delta-of-delta, simple-8b, and RLE for integers and timestamps, Gorilla for floats, dictionary or array for other types | Dictionary is re-encoded as an array when the array would be smaller | None | `tsl/src/compression/algorithms/dictionary.c`, docs |
| InfluxDB 3 | Core: Parquet files. Enterprise engine: `.pt` files | Fixed algorithm per type: delta-delta RLE for timestamps, Gorilla for floats, dictionary for strings | Not documented | Not documented | InfluxData docs (single source) |
| ClickHouse | Column part | The user chains codecs per column with `CODEC(...)`, such as Delta, DoubleDelta, Gorilla, ALP, T64, LZ4, or ZSTD. A server setting picks the method by part size | No automatic yield gate found (unverified) | None | ClickHouse docs (single source) |
| Kafka | Record batch | Producer setting. Attribute bits 0-2 carry the codec | None. It always compresses | `CompressionRatioEstimator` is asymmetric: it moves +0.05 toward a worse ratio and -0.005 toward a better one. It only sizes batches | Kafka protocol docs, `clients/.../CompressionRatioEstimator.java` |
| BtrBlocks | Block of 64,000 values, cascade depth 3 | A stats pass removes schemes that cannot win (RLE if the average run is under 2, frequency encoding if 50% or more of values are unique). It then compresses a 1% sample (10 runs of 64 values) with each remaining scheme | The best estimate wins. Raw is a candidate | None | Kuschewski et al., SIGMOD 2023. Selection takes 1.2% of compression time |
| Vortex | Array | A BtrBlocks-style sample: 16 runs of 64 values (1024 values), or about 1% for large arrays | A scheme competes only if its estimated ratio is **> 1.0**. Otherwise the canonical (raw) encoding stays | None | `vortex-compressor/src/compressor/sample.rs`, `scheme/estimate.rs` |
| ALP | Vector of 1024, group of 100 vectors | Level 1, once per group: a full search of 253 (e, f) pairs on 32 values from each of 8 vectors, keeping the 5 pairs that win most often. Level 2, per vector: 32 values, stops after 2 pairs in a row do not improve. ALP_rd is chosen per group | ALP_rd: cut position p ≥ 48. Dictionary of 1-8 entries, using the smallest size that keeps exceptions at or below 10% | Top-k pairs per group | Afroozeh, Kuffó, Boncz, SIGMOD 2024 |
| pco | Chunk (the caller sets the size) | Picks the mode from a fixed random sample of a few percent. Picks the delta order 1-7 by compressing runs of about 100 values, and stops early | Smallest size | Per chunk | Loncaric, Jeppesen, Zinberg, "Pcodec" |

What the prior art teaches:

1. **Raw is always a candidate, and a minimum saving gates it.** The values cluster at
   10-17%: ZFS 12.5%, RocksDB 12.5%, VictoriaMetrics 10%, QuestDB 1.2x. Vortex uses any
   gain (> 1.0).
2. **Systems use probes and hysteresis only because the probe costs as much as the
   codec.** ZFS probes with LZ4 before zstd, RocksDB samples skipped blocks, and Btrfs
   uses a byte heuristic. With light numeric codecs the probe is a stats pass that is
   cheaper than the codec, so a skip state saves almost nothing.
3. **Sticky and early decisions fail.** Btrfs `NOCOMPRESS` turns compression off for the
   whole file after one bad range. parquet-java fixes the dictionary choice from the
   first page. Both keep a bad choice after the data changes. A decision per vector has
   no such state.
4. **Byte-level heuristics are built for byte-oriented codecs.** Section 3 shows that
   they mispredict numeric codecs by up to 300x.

## 2. How sensor data behaves

**Noise sets the floor.** Gaussian noise of σ LSB has an entropy of about log2(σ) + 2.05
bits/value. A frame of reference over 1024 values spans about ±3.2σ, so FFOR spends
about log2(σ) + 2.7 bits, plus up to one bit of rounding. Delta of independent noise
multiplies σ by √2, which costs 0.5 bit more. Delta wins when the signal moves more
within a vector than the noise does. FFOR wins when noise dominates. The per-vector
selector finds the winner: on `adc16.s16` it picked FFOR for 16 vectors and delta for
240. Bit-packing stays 1-2 bits/value above the order-0 entropy, because it uses one
width for the whole vector. Only an entropy coder (pco) closes that gap.

**The container is wider than the signal.** A 16-bit ADC with an ENOB of 12 has about
4-5 LSB of noise. That gives about 5-6 bits/value, which is 2.7-3x. A 24-bit delta-sigma
ADC stored in `i32` gives 3-5x. Yield goes to 1.0 only when the noise fills the
container (white noise or random data) or when the format destroys the structure
(calibrated real doubles).

| Data class | Benchmark set | Best light ratio | Where yield collapses | Notes |
|---|---|---|---|---|
| ADC counts, 16-bit | `adc16.s1` / `.s16` / `.s256` / `.white` | 3.96 / 2.00 / 1.36 / 1.00 | Only full-range white noise | pco: 5.16 / 2.28 / 1.47 / 0.99 |
| ADC counts, 24-bit in `i32` | `adc24.s6` / `.s100` | 4.65 / 3.15 | Not reached | pco: 5.21 / 3.28 |
| Fast periodic signal (20 samples per period) | `adc16.vib` | 1.23 | Partly. First-order delta cannot follow it | pco: 2.63, or 3.45 at 64k. pco's higher delta orders follow the curve |
| Calibrated real doubles | `f64.cal` / `f64.calp` / `f32.cal` | 1.61 / 1.80 / 3.01 (fdelta) | ALP 0.88-1.07 and ALP_rd 1.02-1.17 fail. fdelta fails at zero crossings | pco: 1.68 / 1.84 / 3.51. The same data as `i32` counts gives 4.65 |
| Decimal process values | `f64.dec2` | 14.87 (ALP) | Not reached | pco: 15.08 |
| Slow values and setpoints | `f64.slow` / `f64.step` | 224 / 561 (RLE + ALP per vector) | Not reached | Already under 0.3 bits/value |
| Enum state | `u8.state` | 273 (FFOR width 0 + RLE) | Not reached | |
| Timestamps, hardware clock | `ts.fixed` / `ts.rational` | 455 / 77.5 | Not reached | 0.14-0.83 bits/value |
| Timestamps with jitter | `ts.ptp` (50 ns) / `ts.soft` (200 µs) | 7.00 / 3.17 | Jitter is entropy | pco: 7.20 / 3.18. No codec helps |

Findings:

- **Real doubles are the one class where codec choice decides the budget.** A
  calibration polynomial turns 21 effective bits into 64-bit values whose low mantissa
  bits are noise. Sortable-bits delta (`fdelta`) keeps the time smoothness and comes
  within 2-17% of pco's size at under 1/100 of the CPU. ALP_rd never beats fdelta on
  this data. The bigger lever is outside the codec: as `f32` the same signal costs 1.33
  bytes/sample, and as `i32` counts it costs 0.86. This is a data-model question and
  outside this report.
- **Short series keep most of their yield with light codecs.** At 10 samples per series,
  FFOR on `adc16.s16` costs 1.21 bytes/sample against 2 for raw. zstd costs 3.40 and LZ4
  costs 2.70, which is worse than raw. VictoriaMetrics skips blocks under 128 bytes for
  the same reason.
- **Jittered timestamps cost 9-20 bits/sample.** That is noise, and no codec removes it.

## 3. Benchmark

**Setup.** Apple M3 Max, macOS (Darwin 27.0), rustc 1.98.1. One thread, release build
with `lto = "fat"`, `codegen-units = 1`, and no `target-cpu` flag. Crates: `alp` 0.0.4,
`fastlanes` 0.7.2, `pco` 1.0.3 (default level 8), `lz4_flex` 0.14.0, and `zstd` 0.14.0
(libzstd 1.5.7). Each dataset has 256 vectors of 1024 values, made with a fixed random
value. Each time is the best of 5 windows of at least 25 ms, so the data is warm in
cache. The harness checks every decode bit for bit against the input. Harness:
`scratchpad/compress-bench/`. Full tables: `compress-bench/out.md`. Run: `cargo build
--release && ./target/release/compress-bench > out.md` (about 80 s).

Codec names: `raw` is a plain copy. `ffor` is FastLanes FOR plus bit-packing. `delta` is
natural-order delta plus FOR plus bit-packing, decoded with a serial prefix sum.
`delta_fl` is the FastLanes transposed delta, with lane bases coded as deltas. `stride`
is the residual from the exact line `t0 + floor(i * span / 1023)`. `fdelta` maps float
bits to order-preserving integers and then applies `delta`. `/64k` means one codec call
per 64 vectors instead of per vector.

### 3.1 Ratio

| Dataset | ffor | delta | delta_fl | rle | stride | alp | alp_rd | fdelta | lz4 | zstd1 | pco | zstd1/64k | pco/64k |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| adc16.s1 | 1.55 | **3.96** | 3.40 | 0.61 | | | | | 1.04 | 1.33 | 5.16 | 1.03 | 5.23 |
| adc16.s16 | 1.52 | **2.00** | 1.84 | 0.51 | | | | | 1.00 | 1.26 | 2.28 | 1.02 | 2.43 |
| adc16.s256 | **1.36** | 1.33 | 1.25 | 0.50 | | | | | 0.99 | 1.21 | 1.47 | 1.02 | 1.51 |
| adc16.vib | 1.14 | **1.23** | 1.14 | 0.50 | | | | | 1.05 | 1.31 | 2.63 | 1.50 | 3.45 |
| adc16.white | **1.00** | 1.00 | 0.94 | 0.50 | | | | | 0.99 | 0.99 | 0.99 | 1.00 | 1.00 |
| adc24.s6 | 1.75 | **4.65** | 4.28 | 0.67 | | | | | 1.00 | 1.32 | 5.21 | 1.21 | 5.26 |
| adc24.s100 | 1.75 | **3.15** | 2.98 | 0.67 | | | | | 1.00 | 1.32 | 3.28 | 1.21 | 3.19 |
| f64.cal | | | | 0.80 | | 0.88 | 1.14 | **1.61** | 1.00 | 1.10 | 1.68 | 1.07 | 1.68 |
| f64.calp | | | | 0.80 | | 1.07 | 1.02 | **1.80** | 1.00 | 1.13 | 1.84 | 1.10 | 1.86 |
| f32.cal | | | | 0.67 | | 1.07 | 1.17 | **3.01** | 1.00 | 1.23 | 3.51 | 1.15 | 3.42 |
| f64.dec2 | | | | 1.09 | | **14.87** | 1.30 | 1.48 | 3.42 | 9.20 | 15.08 | 10.81 | 19.57 |
| f64.slow | | | | **157.6** | | 28.86 | 1.30 | 5.59 | 108.3 | 170.5 | 174.3 | 745.3 | 1303 |
| f64.step | | | | **561.0** | | 32.13 | 1.15 | 7.46 | 142.1 | 243.0 | 214.7 | 2752 | 2921 |
| u8.state | 34.13 | 33.03 | 27.04 | **160.6** | | | | | 49.70 | 41.86 | 37.07 | 1197 | 821.8 |
| ts.fixed | 2.13 | **455.1** | 303.4 | 0.80 | 315.1 | | | | 1.32 | 2.84 | 134.3 | 1.37 | 8595 |
| ts.rational | 2.20 | 56.11 | 52.18 | 0.80 | **73.80** | | | | 1.33 | 2.77 | 48.19 | 1.92 | 24.67 |
| ts.ptp | 2.13 | **6.95** | 6.81 | 0.80 | 6.95 | | | | 1.32 | 2.50 | 7.20 | 1.47 | 7.77 |
| ts.soft | 1.88 | 3.04 | 2.99 | 0.80 | **3.17** | | | | 1.21 | 2.04 | 3.18 | 1.34 | 3.28 |

### 3.2 Speed (ns per 1024-value vector over all datasets: min, median, max)

| Codec | Encode | Decode | Median encode, ns/value |
|---|---|---|---|
| raw | 23 / 173 / 210 | 18 / 104 / 138 | 0.17 |
| ffor | 29 / 122 / 422 | 11 / 58 / 149 | 0.12 |
| delta | 59 / 179 / 578 | 311 / 337 / 414 | 0.17 |
| delta_fl | 549 / 706 / 1048 | 535 / 618 / 775 | 0.69 |
| stride | 516 / 612 / 645 | 320 / 335 / 346 | 0.60 |
| alp | 890 / 1128 / 1960 | 205 / 237 / 435 | 1.10 |
| alp_rd | 1207 / 1505 / 1789 | 399 / 564 / 860 | 1.47 |
| fdelta | 328 / 704 / 771 | 402 / 529 / 544 | 0.69 |
| rle (not tuned) | 713 / 1022 / 3403 | 18 / 830 / 1731 | 1.00 |
| lz4 | 174 / 785 / 9660 | 23 / 233 / 3352 | 0.77 |
| zstd1 | 615 / 8076 / 23144 | 54 / 4170 / 9143 | 7.9 |
| pco | 11965 / 83300 / 120460 | 995 / 3280 / 6828 | 81 |
| pco/64k | 5702 / 12985 / 30262 | 424 / 1835 / 3386 | 12.7 |

The natural-order `delta` decodes at about one cycle per value (0.3 ns). On 16-bit and
24-bit counts it is 0.6-1.0 bits/value smaller than `delta_fl`, because the transposed
layout stores one base per lane (64 per vector for 16-bit values). The `delta_fl`
wrapper here is not tuned, so FastLanes can be faster than shown.

### 3.3 Cost of selection and the byte-entropy estimate (ns per vector)

| Dataset | Exact stats pass | ALP level 2 | ALP level 1, per group | Byte entropy, full / sampled 16 of 256 | Ratio predicted by byte entropy | Actual best light ratio |
|---|---|---|---|---|---|---|
| adc16.s1 | 125 | | | 2709 / 286 | 1.34 | 3.96 |
| adc16.s256 | 125 | | | 1546 / 276 | 1.29 | 1.36 |
| adc16.white | 125 | | | 1162 / 498 | 1.01 | 1.00 |
| adc24.s6 | 159 | | | 3172 / 558 | 1.36 | 4.65 |
| f64.cal | 483 | 75 | 43110 | 3287 / 723 | 1.11 | 1.61 |
| f32.cal | 208 | 72 | 19052 | 3180 / 680 | 1.26 | 3.01 |
| f64.dec2 | 482 | 28 | 28928 | 3989 / 356 | 1.71 | 14.87 |
| u8.state | 100 | | | 1874 / 203 | 85.78 | 273 (auto) |
| ts.fixed | 461 | | | 3701 / 520 | 1.45 | 455.1 |
| ts.soft | 459 | | | 3589 / 752 | 1.31 | 3.17 |

The exact pass costs about the same as a sampled byte-entropy estimate (100-490 against
200-750 ns/vec), and it returns exact sizes. Byte entropy predicts what zstd gets, not
what numeric codecs get. It was off by 3x on ADC counts and by 300x on timestamps. ALP
level 1 is the one costly step: 19-49 µs per group, which is 0.19-0.49 µs per vector
over the paper's 100-vector groups.

### 3.4 Auto selector

Per vector: compute exact sizes for raw, FFOR, delta, RLE, and stride (timestamps), or
for raw, fdelta, and RLE (floats), plus the ALP estimate. Pick the smallest. Store raw
if the best size is above `raw * (1 - θ)`. If ALP wins on its estimate, encode it and
fall back to the best exact candidate if the real size is larger. Results for θ = 1/8:

| Dataset | Picks (vectors) | Ratio | Best single codec | With outer zstd1 per 64 vectors | Select ns/vec | Encode ns/vec (with select) | Decode ns/vec |
|---|---|---|---|---|---|---|---|
| adc16.s1 | delta 256 | 3.96 | 3.96 | 5.72 | 126 | 235 | 312 |
| adc16.s16 | ffor 16, delta 240 | 2.00 | 2.00 | 2.31 | 123 | 234 | 288 |
| adc16.s256 | ffor 253, delta 3 | 1.36 | 1.36 | 1.35 | 127 | 206 | 42 |
| adc16.vib | delta 256 | 1.23 | 1.23 | 1.28 | 127 | 249 | 324 |
| adc16.white | raw 256 | 1.00 | 1.00 | 0.99 | 128 | 156 | 33 |
| adc24.s6 | delta 256 | 4.65 | 4.65 | 4.64 | 160 | 376 | 341 |
| adc24.s100 | delta 256 | 3.15 | 3.15 | 3.15 | 159 | 375 | 337 |
| f64.cal | raw 8, fdelta 248 | 1.61 | 1.61 | 1.61 | 577 | 1310 | 521 |
| f64.calp | fdelta 256 | 1.80 | 1.80 | 1.81 | 570 | 1333 | 533 |
| f32.cal | raw 8, fdelta 248 | 3.00 | 3.01 | 3.00 | 285 | 656 | 387 |
| f64.dec2 | alp 256 | 14.87 | 14.87 | 16.82 | 522 | 1401 | 234 |
| f64.slow | rle 231, alp 25 | **224.2** | 157.6 | 760.4 | 528 | 1310 | 116 |
| f64.step | rle 256 | 561.0 | 561.0 | 1441 | 562 | 1265 | 127 |
| u8.state | ffor 224, rle 32 | **273.1** | 160.6 | 343.1 | 100 | 228 | 11 |
| ts.fixed | delta 256 | 455.1 | 455.1 | 16913 | 945 | 1344 | 411 |
| ts.rational | stride 86, delta 170 | **77.51** | 73.80 | 3561 | 936 | 1446 | 371 |
| ts.ptp | stride 16, delta 240 | 7.00 | 6.95 | 7.04 | 941 | 1471 | 398 |
| ts.soft | stride 223, delta 33 | 3.17 | 3.17 | 3.18 | 937 | 1524 | 357 |

- **Choosing per vector beats one codec per series** by 42-70% on mixed data
  (`f64.slow`, `u8.state`) and by 5% on rational timestamps. It loses only where θ
  forces raw (`f32.cal`: 3.00 against 3.01).
- **The minimum saving θ.** Moving from 0 to 1/8 changed only 8 near-break-even vectors
  in `f64.cal` and `f32.cal` (from ALP_rd to raw), and the ratio stayed at 1.61 and 3.02
  -> 3.00. A θ of 1/4 lost real yield: `adc16.s256` dropped from 1.36 to 1.09 and
  `adc16.vib` from 1.23 to 1.00. Because the sizes are exact, a rejected vector costs
  only the stats pass. θ trades decode CPU (33 ns/vec for raw against about 300 for
  delta) for bytes.
- **An outer zstd pass** over the light output helps only data that is already small,
  such as clocked timestamps, slow values, and the 1-LSB sine. It does nothing for noisy
  channels (`f64.cal` stays at 1.61, `adc16.s256` at 1.35). Its CPU time was not
  measured.
- **Incompressible data costs 0.11 ns/value extra** (156 ns/vec for auto against 46 for
  a raw copy) to find out that it does not compress.

### 3.5 CPU at the P1 rate (M3 Max, 100M values/s on one core)

| Class | Encode (with select, ALP level 1 per 100 vectors) | Decode |
|---|---|---|
| 8-16-bit counts and state | 0.15-0.24 ns/value, 1.5-2.4% of a core | ≤ 3.2% |
| 24-bit counts in `i32` | 0.37 ns/value, 3.7% | 3.3% |
| `f32` | 0.83 ns/value, 8% | 3.8% |
| `f64` | 1.65-1.76 ns/value, 17-18% | 1.1-5.2% |
| `i64` timestamps | 1.3-1.5 ns/value, 13-15% | 3.5-4% |
| zstd -1 per vector (for reference) | 7.9 ns/value median, 79% | 41% |
| pco per vector / per 64k | 81 / 12.7 ns/value median, 8.1 / 1.3 cores | 3.2 / 1.8 ns/value |

### 3.6 Short series (bytes per sample, wire frames)

| Dataset | Samples | ffor | delta | alp | lz4 | zstd1 | raw |
|---|---|---|---|---|---|---|---|
| adc16.s16 | 10 / 100 / 1000 | 1.21 / 1.01 / 1.31 | 1.36 / 0.95 / 1.00 | | 2.70 / 2.07 / 2.01 | 3.40 / 1.70 / 1.58 | 2 |
| adc24.s6 | 10 / 100 / 1000 | 2.02 / 1.91 / 2.27 | 1.63 / 0.85 / 0.86 | | 4.70 / 4.07 / 4.01 | 5.40 / 3.38 / 3.01 | 4 |
| f64.dec2 | 10 / 100 / 1000 | | | 1.73 / 0.53 / 0.53 | 5.41 / 2.71 / 2.34 | 5.97 / 1.93 / 0.88 | 8 |
| ts.soft | 10 / 100 / 1000 | 4.40 / 3.85 / 4.26 | 4.04 / 2.68 / 2.63 | | 7.48 / 6.68 / 6.61 | 8.02 / 4.31 / 3.92 | 8 |

These sizes come from size arithmetic for natural-order packing of exactly n values with
a compact header. They are not from encoded bytes.

### 3.7 Limits of the benchmark

- The data is synthetic. Real plant data usually has more quantization and repeats,
  which favors RLE and FFOR, so the results for noisy data are conservative.
- The data is warm in cache on one thread, so memory bandwidth is not measured.
- The RLE encoder and the `delta_fl` wrapper are not tuned. RLE is never encoded unless
  it wins, because the stats pass gives its size.
- pco ran at its default level only. The outer zstd pass was not timed.
- The M3 Max is not the target floor. See 3.8.

### 3.8 Re-run on a Raspberry Pi 4

Use a 64-bit OS (aarch64), a heatsink or fan, and the performance governor: `echo
performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor`. Build
with `RUSTFLAGS="-C target-cpu=cortex-a72" cargo build --release`, then run `taskset -c
3 ./target/release/compress-bench > pi4.md 2> pi4.err`. After the run, `vcgencmd
get_throttled` must print `0x0`, or the times are not valid.

Read these numbers:

1. The exact stats pass for 16-bit, 32-bit, and 64-bit types. It is the per-value cost
   of the gate.
2. `delta` decode (a serial prefix sum, 1 add per value) against `delta_fl` decode. This
   decides D5.
3. ALP level 1 per group. If the amortized cost is over about 2 ns/value, D8 needs the
   skip rule.
4. Auto encode and decode per class at θ = 1/8, as a percent of one core at the Pi's
   real target rate.
5. zstd1 and pco per vector, to confirm that they stay off the hot path.
6. The ratios must be identical to the M3 Max run, because the encoders are
   deterministic. A difference is a bug.

If 32-bit Raspberry Pi OS must be supported, also build for
`armv7-unknown-linux-gnueabihf`. On 32-bit ARM, 64-bit operations (timestamps, `f64`)
are much slower, and the `fastlanes` crate has no test there.

I estimate that the Pi 4 is 5-10x slower per core than the M3 Max on this code (1.5 GHz
Cortex-A72 against a 4 GHz core). This estimate is not verified.

## 4. Recommendation

### 4.1 Selection algorithm

1. **Unit:** one vector of 1024 values. A series is a sequence of vectors. The last
   vector can hold fewer values, and it uses natural-order packing of exactly n values
   so that short wire frames keep their yield.
2. **One stats pass per vector** computes min and max, the min and max of neighbor
   deltas, and the count of equal neighbors. Floats use the same pass over sortable
   bits. Timestamps add the stride residual range. These values give the **exact** size
   of raw, FFOR (width 0 is the constant case), delta, RLE, stride, and fdelta.
3. **ALP** gets an estimate from level 2 (32 values, early exit) over the channel's
   cached top-5 exponent pairs. If ALP wins on its estimate, encode it. If the real size
   is larger than the best exact candidate, use that candidate instead.
4. **Raw is always a candidate.** If the best size is above `raw * 7/8`, store raw.
5. **No sampling and no hysteresis for the light codecs.** The decision is exact and has
   no state, so there is nothing to tune. The only state is ALP's top-5 pairs per
   channel. Refresh them after each 100 vectors' worth of values, counted across series
   so that short frames also get a refresh.

### 4.2 Codec set

- Integers: raw, FFOR, delta, RLE.
- Timestamps: the integer set plus stride.
- Floats: raw, ALP, fdelta, RLE.
- `max` mode adds pco per vector for all types.
- Compared with r2: add fdelta, remove ALP_rd, use natural-order delta, and drop zstd.

### 4.3 Signaling

Each vector starts with a 1-byte codec tag and a 1-byte width, followed by the codec
fields (reference, first value, `t0` and span, ALP exponents and exception count, run
count, or tail count). Pad the header so that a full vector's payload is aligned to the
sample width. That keeps FastLanes reads in place, and the padding costs under 0.06
bits/value. The tag costs 0.008 bits/value. Sending the tag only when it changes would
save at most that much, but it would make a vector depend on earlier vectors and break
the rule that each encoded series decodes by itself.

The tag replaces "compression known per channel" in S2. The reader needs only the
channel's data type and the tags. It does not need the policy, so a policy change needs
no coordination or migration. In `raw` mode every vector carries the raw tag, and its
payload stays a contiguous typed slice that can be read in place.

### 4.4 Validation at the home

For the light codecs, the home validates only the headers. It checks that the tag is
known, that the width is not more than the type width, and that the payload length
equals the length that the header implies. It also checks that RLE run lengths sum to
the count, that ALP exponents are in range, and that exception positions are under
1024. It does not decode. A pco vector (`max` mode only) needs a full, bounded decode to
      validate. All decoders take input from outside, so they need fuzz tests.

### 4.5 Policy shape (S12 style)

```toml
# No block is needed: every index is "auto" by default.

[[compression]] select = "lab.scope_*.**" mode = "raw"     # never compress; vectors can
be read in place

[[compression]]
select = "site_b.**"
mode = "max"     # add pco; the writer pays the CPU
```

- `mode`: `auto` (default), `raw`, or `max`. The policy has no other fields.
- Like the other S12 policies, it selects indexes. Data channels follow their index, the
  most specific pattern wins, and equal specificity is a plan error. `explain` shows the
  effective mode. `auto` already adapts per vector, so per-channel overrides are not
  needed.
- The writer encodes. A CPU-starved writer (or a Pi) uses `raw`. A thin link or cold
  archive uses `max`.

### 4.6 What not to do

- Do not use byte-entropy heuristics (Btrfs style) for numeric data.
- Do not keep sticky "do not compress" flags (Btrfs) or decide once from the first block
  (parquet-java).
- Do not run zstd or LZ4 on numeric vectors. They get 0.99-1.33x on noisy data at 10-50x
  the CPU, and on short frames they make the data larger than raw.

## 5. Decisions

**D1. Selection method.** Choose per 1024-value vector from exact sizes out of one stats
pass, with raw always a candidate. Use sampling only for ALP exponents. *Recommendation:
yes.* It is exact, stateless, and costs 0.10-0.50 ns/value.

**D2. Minimum saving.** Make it a fixed 1/8 and do not expose it in the policy.
*Recommendation: yes.* 1/8 lost no yield here, 1/4 lost real yield, and ZFS and RocksDB
use 1/8. One less setting.

**D3. Signaling.** Put a 1-byte codec tag in every vector, always, instead of "only on
change" or "known per channel" (S2). *Recommendation: yes.* It costs 0.008 bits/value,
and every series decodes by itself.

**D4. Float codecs.** Add fdelta and drop ALP_rd. *Recommendation: yes.* fdelta gives
1.61-3.01x on calibrated values, where ALP_rd gives 1.02-1.17x and never wins past the
1/8 minimum saving. The evidence is synthetic, so test with recorded plant data before
the format freezes.

**D5. Delta layout.** Use natural-order delta with a serial prefix-sum decode, not the
FastLanes transposed delta. *Recommendation: yes, after the Pi 4 check.* It is 0.6-1.0
bits/value smaller on counts and decodes at 0.3 ns/value on the M3 Max. If the Pi 4
decode is over about 2 ns/value, revisit.

**D6. Policy.** Use a `[[compression]]` policy with `select` and `mode = auto | raw |
max`, with `auto` as the default and no other fields. *Recommendation: yes.*

**D7. Contents of `max`.** `max` adds pco per vector as a candidate. The writer encodes
it, and the home validates it by decoding. *Recommendation: yes, opt-in only, built
after `auto`.* pco saves 2-23% more bytes on noisy data (53% on fast periodic signals)
at 70-550x the CPU. If `max` is mostly for thin links, the transmission policy (B6) may
be a better home for it. This report does not decide that.

**D8. ALP state.** Refresh the top-5 exponent pairs after each 100 vectors' worth of
values, always, with no skip rule. *Recommendation: yes for now.* It costs 0.2-0.5
ns/value on the M3 Max. If the Pi 4 shows more than about 2 ns/value, add a
RocksDB-style skip: if a group picked ALP for no vector, skip level 1 and try it again
every 8th group.