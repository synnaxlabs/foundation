# Foundation research fork 2: storage

Scope: S3 (variable-length layouts), the S7 optional-field gap, S4 (disk format),
codecs, and re-indexing. Date: 2026-10-04. Sources are linked inline. "Unverified" marks a claim with one
source or a design estimate.

One correction to the design notes first: Synnax no longer separates strings with
newlines. Since SY-4059 (commit 2865e72aa7, April 2026) every variable-length sample
has a 4-byte length prefix in front of it (`x/go/telem/series_factory.go:129`,
`x/go/telem/series.go:52-66`). The S2 note "Synnax newline separation breaks for binary"
describes the old format. The lesson still applies, with a different cause (see Q1).

## Q1. S3: strings, bytes, and lists in one buffer

**Recommendation:** every variable-length series is `ends[n] ++ data`: n end offsets
(u32), then the bytes; the sample count n comes from the frame, once per index group,
never from the series.

### Layouts per type

| Type | Layout in one buffer | Sample i |
|---|---|---|
| primitive, enum, flags | `values[n]`, fixed width | `values[i]` |
| fixed array `T[N]` | `values[n * N]`, row-major | `values[i*N .. (i+1)*N]` |
| string, bytes | `ends[n]: u32` then `data` | `data[ends[i-1] .. ends[i]]` (ends[-1] = 0) |
| `list<T, max>` | `ends[n]: u32` then the child layout of T | child elements `ends[i-1] .. ends[i]` |

- `string` and `bytes` share the layout. UTF-8 is checked once at the writer edge,
  never again on the hot path.
- Lists of structs do not exist (S7 turns them into one list per field), so a list
  child is always a primitive, enum, flags, fixed array, string, or bytes. A
  `list<string>` nests: outer ends, then inner ends, then data.
- In memory, a sliced series keeps a `base` offset next to the buffer (an in-memory
  field, never sent), so slicing never copies. Arrow solves the same problem with an
  explicit first offset ([Arrow columnar format](https://arrow.apache.org/docs/format/Columnar.html#variable-size-binary-layout)).
- Validation is O(n) and branch-light: ends are non-decreasing, and the last end
  equals the data length.

### Why ends, not prefixes or separators

- **Separators** (old Synnax newlines) cannot carry binary data without escaping, and
  escaping makes every read a scan.
- **Interleaved length prefixes** (Synnax today) make `Len()` and `At(i)` scans over the
  whole buffer (`x/go/telem/series.go:52-66`, `:199-216`; Synnax caches the length to
  hide it). Prefixes mixed into the data also block integer compression of the lengths
  and SIMD passes over the bytes.
- **Separate ends** give O(1) access, compress as a monotonic integer column (delta
  plus bit-packing, Q4), and leave the bytes contiguous for UTF-8 checks and optional
  string codecs. Parquet stores all lengths first and then all bytes for the same reason
  ([Parquet DELTA_LENGTH_BYTE_ARRAY](https://parquet.apache.org/docs/file-format/data-pages/encodings/)).
  Arrow uses offsets plus data. Two independent formats agree.
- `u32` ends cap one series at 4 GiB, far above any frame. A per-channel maximum sample
  size (the A13 idea applied to strings and bytes) bounds buffers at the edge.

### The sample count

The receiver needs n before it can find where `data` starts. Options:

1. **One count per index group in the frame (recommended).** All series on one index
   have the same length in a frame (A7, S1), so one number covers all of them. It is
   framing, like the key set, not series payload. It also removes every per-series
   length from the uncompressed frame: a fixed-width series is `n * width` bytes, and a
   variable-length one is `4n + ends[n-1]` bytes. The stateful codec (A8) can predict n
   for fixed-rate indexes, so most frames send no count at all. The group names its
   index key, so a frame decodes the same way before and after a re-index (Q5).
2. Always ship the index series with its data channels. It wastes bandwidth for readers
   that only want latest values, and a reader without the index still could not decode.
3. A count inside each variable-length series. It is a per-series header, which S2
   rejects.

Risks: none beyond the frame codec, which another fork owns. The count rule must be
part of the wire spec.

**Decision for the user:** do variable-length series use `ends[n] ++ data`, with the
sample count carried once per index group in the frame?

## Q2. The S7 optional-field gap

**Recommendation:** an optional field's presence is per frame: a writer cuts a new
frame when presence changes, and a typed struct view is always assembled from the
samples of one frame at one timestamp, never from the latest value of each field.

### Why the gap exists

S7 turns `motor_1: MotorState` into one channel per field on one index. Two failure
modes follow:

1. **Stale pairing.** A reader that takes "the latest value of each field" pairs a new
   `speed` with a `fault` from an older sample.
2. **Absent vs not written.** Inside a frame with 100 timestamps, a field present at
   samples 3 and 7 only cannot be expressed, because series on one index must have equal
   length (A7).

### How others handle it

- **OPC UA** encodes optional fields with a 32-bit EncodingMask per value, one bit per
  optional field, at most 32 ([Part 6, 5.2.7](https://reference.opcfoundation.org/Core/Part6/5.2.7/)).
  Presence is per value.
- **Sparkplug B** gives each metric an `is_null` flag
  ([Ignition SDK Metric protobuf](https://files.inductiveautomation.com/sdk/javadoc/ignition83/8.3.0/com/cirruslink/sparkplug/protobuf/SparkplugBProto.Payload.Metric.html)).
  Presence is per metric per message.
- **Arrow and Parquet** use a validity bitmap per column. Presence is per value, in a
  side buffer.
- **Ignition UDT** members are separate tags that update on their own, so reading an
  instance is exactly the stale-pairing case (unverified: from product knowledge, no
  second source fetched).
- **PI** marks each point stepped or interpolated (the Step attribute), which decides
  how readers join values across time
  ([AVEVA glossary: Step attribute](https://docs.aveva.com/bundle/glossary/page/step_attribute.html.html)).

### The fix

1. **Struct views join by exact timestamp within one frame.** The SDK's generated
   `MotorState` view reads a frame and builds one struct per timestamp of the index
   series. A field channel missing from the frame is `None` for every sample in it.
   This needs no new machinery: B4 already keeps whole frames per index for latest
   readers, and a new latest reader gets the newest whole frame, so "absent now" reads as
   `None`, not as a stale value.
2. **Presence changes cut frames.** The generated writer (the only writer S7 trusts to
   send whole structs) starts a new frame whenever the set of present optional fields
   changes. Connector `in` entries are grouped by index (one group = one clock = one
   writer), so each group's writer cuts its own frames independently. Sources with optional fields are low-rate in practice: OPC UA structures and
   events arrive one value per notification, so they already are one sample per frame.
   High-rate DAQ data has no optional fields.
3. **Absent means null by definition.** The home cannot tell "omitted on purpose" from
   "forgot to write", and does not try. S7 already accepted that only generated writers
   send whole structs.

Rejected alternatives:

- **Validity bitmap side array:** lossless per sample, but it is a per-series side
  structure, which S2 and S13 removed.
- **Sentinel values** (NaN, i64::MIN): not lossless (NaN is a real value), and no
  sentinel exists for bool or u8.
- **Optional field on its own sparse index:** lossless per sample and data only, but it
  breaks S7's "one index per struct", and readers must join two indexes by exact time.
  Keep it as the escape hatch for a source that flips presence at high rate.
- **As-of join of fields** (the S13 rule for quality): correct for step channels like
  quality, wrong for struct fields, which are point values.

Risk: a non-generated writer that sends fields of one struct in two frames at the same
timestamps produces two half-structs. Documented, not enforced.

**Decision for the user:** is optional-field presence per frame, with struct views built
from one frame at a time and never from each field's latest value?

## Q3. S4: the disk format

**Recommendation:** each shard keeps a preallocated, checksummed write-ahead ring for
frames not yet flushed, plus immutable columnar segment files (one chunk per channel,
index at the end); eviction deletes whole segments; there are no per-channel files.

### The design

1. **Write-ahead ring.** A fixed set of preallocated files per shard. A record is
   `[len: u32][crc32c: u32][payload]`, where the payload is one group commit's frames in
   the plain S3 layout (or the light per-record codec). Group commit every few ms (B1),
   then one data sync of the current file: `fdatasync` on Linux, `F_FULLFSYNC` on macOS,
   `FlushFileBuffers` on Windows. Rust's `File::sync_all` and `sync_data` already use
   `F_FULLFSYNC` on Apple targets
   ([rust-lang/rust#60121](https://github.com/rust-lang/rust/pull/60121);
   [macOS fsync(2)](https://opensource.apple.com/source/xnu/xnu-4570.61.1/bsd/man/man2/fsync.2.auto.html)).
   Because the files are preallocated and reused, the hot path never creates, renames,
   or deletes a file, so it never needs a directory sync. That matters on Windows, where
   a directory cannot be flushed like a file
   ([Old New Thing](https://devblogs.microsoft.com/oldnewthing/20170510-00/?p=95505)).
   TigerBeetle avoids the same problem with one preallocated data file
   ([TigerBeetle data file](https://docs.tigerbeetle.com/about/internals/data_file)).
2. **Memtable.** Each channel's recent samples stay in pool buffers until a flush. Latest
   readers and fresh complete readers are served from here (B3, B4).
3. **Columnar segment file.** When the ring passes a size or age threshold, the shard
   writes one immutable segment of **chunk groups, one per index**. The group header
   names the index key and its seq range, and the index's timestamp chunk comes first.
   One chunk per data channel follows, encoded per Q4, with its own CRC and the seq runs
   of the index it covers (usually one run). The runs make an absent optional field
   (Q2) or a B5 gap cost one run, not a null per sample. A footer maps each channel key
   to its chunks, sorted by key. Grouping by index also makes re-indexing free (Q5).
   Then sync the file, sync the parent directory once (the Linux rule:
   [LWN](https://lwn.net/Articles/667800/)), and release the ring space. Apache TsFile
   is the closest prior art: chunks per series, chunk groups per device, index at the end
   ([TsFile format](https://cwiki.apache.org/confluence/display/IOTDB/TsFile+Format)).
   A TsFile device is one writer, which matches our index group (one group = one clock
   = one writer). InfluxDB 3 (WAL then Parquet), QuestDB (WAL then column files,
   [storage engine](https://questdb.com/docs/architecture/storage-engine/)), and IoTDB
   (WAL, memtable, TsFile) all use the same two-tier shape.
4. **Eviction.** Delete the oldest segment file whole once every holding reader has
   passed it, or once retention ends (B1, S10). Deleting a file is cheap; deleting
   samples inside one is not.
5. **Repair on open.** Scan the ring from the last flush, check each CRC, and truncate at
   the first torn or bad record. Check each segment footer; a bad chunk becomes an
   explicit gap for its channel (the B1 gap rule), not a lost segment.
6. **Errors.** Any sync error is fatal for that file: stop, reopen, recover from the
   ring. Never retry a failed fsync. PostgreSQL learned this in 2018: Linux can drop the
   dirty pages and report success on the retry
   ([PostgreSQL wiki: Fsync Errors](https://wiki.postgresql.org/wiki/Fsync_Errors);
   [danluu: fsyncgate](https://danluu.com/fsyncgate/)).
7. **Catch-up reads** of old data send segment chunks as they are on disk, in a
   "block mode" where codec state resets per chunk. This keeps S2's "serve catch-up
   without re-encoding".

### What S2's "one byte format" means after this

The plain S3 layout is shared by memory, uncompressed links, and decoded chunks.
Compressed bytes are shared by disk chunks and catch-up streams. Live wire bytes are
different, because the A8 codec carries state across frames per connection, and stateful
bytes cannot be stored and replayed to a reader that starts in the middle. I recommend
recording this split explicitly in S2 rather than chasing one compressed format for all
three.

### Per-channel cost

- Disk: zero files and zero descriptors per channel. A channel costs one footer entry in
  each segment where it has data.
- Memory: one memtable slot per active channel (key, buffer handle, codec state, latest
  frame reference). Estimate: 100 to 200 bytes per channel, so 1M active channels need
  100 to 200 MB (unverified design estimate, to benchmark). A Raspberry Pi node (P1: idle
  under 50 MB) will home thousands of channels, not millions.
- Low-rate channels flush small chunks, and each chunk carries a few bytes of codec
  parameters. Flushing by size or age, not per commit, keeps chunks large: a 1 Hz
  channel flushed every 10 minutes has 600 samples per chunk.

### Rejected

- **Per-channel files (Synnax Cesium).** One directory per channel
  (`cesium/open.go:144`, `cesium/delete.go:64`), each with an index file, a counter file,
  data files of up to 800 MB, and up to 100 descriptors
  (`cesium/internal/domain/db.go:84-90`). That is millions of directories at our scale.
  Cesium also never syncs (`Sync()` appears only in test helpers,
  `x/go/io/fs/testutil/fs.go:116`) and has no checksums in production code.
- **One file per column per partition (QuestDB).** "One or two files per column per
  partition" ([QuestDB](https://questdb.com/docs/architecture/storage-engine/)) is a file
  explosion at millions of channels.
- **Row log only (Kafka).** No columnar compression, and Kafka relies on replication
  instead of fsync by default
  ([Kafka server.properties](https://apache.googlesource.com/kafka/+/HEAD/config/server.properties)).
  Our homes may have no replica.
- **LSM tree (RocksDB, fjall).** Built for random keys and updates; time-series appends
  pay compaction write amplification for merges they never need, and a key per sample
  wastes space. A small embedded store (fjall 3.1, redb 4.3) may still hold tiny metadata
  such as reader positions; that is another fork's call.

### Page cache, O_DIRECT, io_uring

- **Buffered I/O plus explicit syncs in v1.** It works the same on Linux, macOS, and
  Windows, and the page cache serves catch-up reads of recent segments for free.
- **O_DIRECT later, Linux only, behind benchmarks.** It removes double buffering
  (TigerBeetle and Redpanda manage their own cache), but it needs aligned buffers and an
  own cache, and macOS has no equivalent (`F_NOCACHE` is advisory).
- **io_uring is a runtime question (C2).** The storage engine stays sans-I/O: it emits
  "append these bytes, sync this file" and a per-OS driver executes them, so the T1
  simulator can run it unchanged. compio covers io_uring, IOCP, and kqueue; Glommio has no
  Windows support ([compio docs](https://docs.rs/crate/compio/latest)).

Risks:
- Retention granularity becomes the segment. A segment is kept until every index in it
  may go, so one index with a long hold keeps the whole segment. Mitigation: separate
  ring and segment groups per retention class, if benchmarks show waste.
- Flush is a CPU and I/O burst. Size thresholds must be tuned against P1.

**Decision for the user:** two tiers per shard (a preallocated, checksummed write-ahead
ring, plus immutable columnar segments with one chunk per channel), whole-segment
eviction, and no per-channel files?

## Q4. Codecs

**Recommendation:** build four small codecs ourselves (rational stride for timestamps,
ALP for floats, FastLanes delta and frame-of-reference bit-packing for integers and
`ends`, run-length for step channels) and use them on both the wire and disk; keep pco
and zstd as optional, benchmark-gated codecs for cold segments and weak links.

### Measurements

ALP paper, Table 4 (13 time-series datasets of doubles, bits per value, lower is better)
and Table 5 (Ice Lake, values per CPU cycle)
([ALP, SIGMOD 2024](https://ir.cwi.nl/pub/33334/33334.pdf)):

| Codec | Bits/value, time series | Compress, values/cycle | Decompress, values/cycle |
|---|---|---|---|
| Gorilla | 48.1 | 0.052 | 0.047 |
| Chimp | 42.6 | 0.042 | 0.039 |
| Chimp128 | 24.5 | 0.040 | 0.040 |
| Patas | 30.9 | 0.060 | 0.157 |
| Elf | 18.2 | 0.010 | 0.012 |
| Zstd | 17.2 | 0.035 | 0.101 |
| **ALP** | **16.4** (13.2 with dictionary or RLE first) | **0.487** | **2.609** |

At 3 GHz, ALP compresses about 1.4 billion doubles per second per core and decompresses
about 7.8 billion. P1's 100M samples/s per node costs under a tenth of one core to
encode. DuckDB made ALP its default and deprecated Chimp128 and Patas
([DuckDB ALP](https://duckdb.org/library/alp/)), a second source for the ranking.

- **FastLanes**: bit-packing, frame of reference, delta, and RLE in a layout that
  decodes over 40 values per cycle with scalar code
  ([VLDB 2023](https://vldb.org/pvldb/volumes/16/paper/The%20FastLanes%20Compression%20Layout%3A%20Decoding%20%3E100%20Billion%20Integers%20per%20Second%20with%20Scalar%20Code);
  [fastlanes crate README](https://docs.rs/crate/fastlanes/0.6.1/source/README.md)).
  ALP's integer stage is FastLanes FFOR, so the two share code.
- **pco**: 29% to 94% better ratio than other numerical codecs on six datasets, over
  1 GiB/s decode per thread ([Pcodec paper](https://arxiv.org/html/2502.06112v2)), but it
  wants more than 10k numbers per chunk and more than 1k per page
  ([pcodec README](https://raw.githubusercontent.com/pcodec/pcodec/main/README.md)). It
  fits cold segment chunks, never live frames.
- **Gorilla, Chimp, Patas, Elf**: serial bit-level decoding with branches; 9x to 215x
  slower than ALP and a worse ratio (Elf is the exception on ratio, at 215x slower
  decode). Rejected.
- **BtrBlocks**: a sampling framework that picks cascades of light codecs per block.
  Vortex, built on its ideas, claims files 38% smaller and 10x to 25x faster to decode
  than Parquet with zstd ([Vortex blog](https://vortex.dev/blog/btrblocks-compressor);
  vendor claim, unverified). The idea (sample a block, pick the codec) is worth copying
  for choosing between ALP, FFOR, and RLE per 1024-value vector; the framework itself is
  more than we need.

### Codec per channel kind

| Data | Codec | Wire | Disk |
|---|---|---|---|
| Index timestamps | Rational stride (start, num/den, n) plus patched exceptions; else delta-of-delta then FastLanes bit-packing | yes | yes |
| f32, f64 | ALP per 1024 values, ALP_rd fallback for non-decimal vectors | yes | yes |
| Integers, raw counts, enums, flags | FastLanes FFOR or delta then bit-pack | yes | yes |
| Step channels (quality, control, error) | Run-length | yes | yes |
| string, bytes | `ends` via FastLanes delta; data raw, or LZ4 on disk | yes | yes |
| Cold numeric chunks | pco (optional) | no | benchmark-gated |
| Weak links | zstd as an outer layer (B6 compression level) | per link | no |

The rational stride is our own idea, not prior art (unverified novelty): a DAQ at
3 samples per millisecond has a stride of 333,333.33 ns, which no integer stride encodes
exactly, but num/den = 1,000,000/3 does.

### What P1's "under 4 bytes per sample" depends on

- ALP's 16.4 bits (about 2 bytes) holds for doubles that came from decimals. On "real
  doubles" with no decimal origin it falls to ALP_rd at about 55 bits (POI datasets,
  Table 4). A float produced by a calibration polynomial is a real double.
- So P1 holds when channels carry raw integer counts (a 16-bit ADC bit-packs to 2 bytes
  or less) and scale through a calculation (A17), or when sources report decimal values,
  as most OPC UA and Modbus sources do. Fixed-rate timestamps add almost nothing, and they
  are shared by every channel on the index (A7).
- This is a strong argument for keeping raw counts on the wire by default, which A17
  already allows.

### Rust implementations and build vs adopt

| Crate | Version | Notes |
|---|---|---|
| `alp` (spiraldb) | 0.0.4 | Pre-1.0, both ALP and ALP_rd |
| `fastlanes` (spiraldb) | 0.7.2 | Pre-1.0 |
| `vortex-alp`, `vortex-fastlanes` | 0.87.0 | Tied to the Vortex array model |
| `pco` | 1.0.3 | Stable format, used by Zarr and CnosDB |
| `crc32c`, `crc-fast` | 0.6.8, 1.10.0 | Hardware CRC32C on x86 and ARM |

(crates.io API, 2026-10-04.)

- **Build ALP, FastLanes kernels, stride, and RLE ourselves.** Each is a few hundred to
  a few thousand lines of vector kernels, they sit on the hottest path, and we need them
  allocation-free over our pool buffers. Use the spiraldb crates and the C++ reference as
  differential test oracles (T1 layer 1 and fuzzing).
- **Adopt pco** if benchmarks justify it: its tANS stage is complex and its format is
  stable at 1.0.
- **Adopt a CRC32C crate.** Trivial, hardware-accelerated, no I/O.

Risks:
- ALP and FastLanes papers measure L1-resident vectors. End-to-end numbers on our frames
  must come from our benchmarks (T1 layers 4 and 5).
- Own codecs carry correctness risk; property tests and fuzzing against the reference
  crates are mandatory.

**Decision for the user:** four own codecs (stride, ALP, FastLanes, RLE) on wire and
disk, pco and zstd only as benchmark-gated options, and raw counts as the default way to
meet P1's bytes-per-sample target?

## Q5. Re-indexing a data channel

Requirement (added by the coordinator): a data channel can move to another index
(new rate, split or merged groups) and keep its key and name. Stored samples keep their
original timestamps. Readers see one continuous channel. Placement and retention follow
the index (S12), so a re-index can move the channel's home; history stays at the old
home until its retention ends.

**Recommendation:** a re-index is a committed epoch, not a data move: the channel
record keeps a short list of `(index, from: T)` epochs, the old index stores the
channel's samples before T and the new index from T, and every stored chunk records its
index through its chunk group, so nothing is ever rewritten.

### What the Q3 format already gives

- **The disk records the index of every stored range, at zero cost.** A data chunk sits
  inside the chunk group of the index it was written with, and it can only be read
  against that group's timestamp chunk. Repair on open can check each data chunk's
  runs against its group's seq range without reading channel metadata.
- **The write-ahead ring is self-describing too.** Each record names its index groups
  (index key, sample count, member keys), the same framing as Q1. A record written
  before a re-index decodes with the grouping it was written with, never with the
  current metadata.
- **Segments may hold both epochs.** If the cut falls inside one segment, the channel
  has one chunk in group A (runs before T) and one in group B (runs from T).

### The procedure

1. The user changes `Kind::Data.index` from A to B. The mesh commits the change with
   one cut time T and appends `(B, from: T)` to the channel's epoch list. A split or a
   merge moves many channels in one change with one T.
2. The connector config changes: the `in` group for A drops the channel and the group
   for B adds it (one group = one clock = one writer).
3. A's home accepts the channel under A only for timestamps before T. B's home accepts
   it under B only from T. A late write on the wrong side is dropped and stored as a
   gap (B5: live writes never wait). So the epochs never overlap, by construction.
4. History stays under A at A's home. When A's retention removes the channel's last
   chunk under A, the epoch entry is pruned.

### Readers

- **History:** a read over `[t0, t1)` splits the range at epoch boundaries and asks
  each epoch's home. Each piece carries the timestamps of its own index. The SDK joins
  the pieces into one continuous stream of `(time, value)`.
- **Live and durable readers:** reader positions stay per index (A6). A reader follows
  the channel under A until A's seq passes T, then continues under B from B's first
  seq at or after T. In complete mode (B3) it finishes A's synced data before it starts
  B. A hold (S10) on the old epoch stays at A's home and is capped by A's retention.
- **Latest readers** read only the current epoch.

### Costs

| Item | Cost |
|---|---|
| Re-index operation | One metadata change. No copy, no rewrite. O(1) in history size. |
| Disk mapping | Zero bytes: the chunk group names the index. |
| Seq runs per data chunk | About 16 bytes per chunk (start seq, count), also needed by Q2 and B5. |
| Ring | One index key and count per group per record, also needed by Q1. |
| Channel metadata | One epoch (index key plus timestamp, about 16 bytes) per re-index inside retention. |
| Reads across the cut | One more request per epoch crossed, maybe to a second home. |
| Memory | Briefly two memtable slots for the channel, until A's group flushes. |
| Data at the cut | A possible gap, bounded by the clock offset between A and B plus change propagation delay. |

### Rejected

- **Rewrite history under the new index** (copy or resample): breaks "keep the original
  timestamps", costs O(history), and moves data between homes.
- **Timestamps in every data chunk:** makes re-indexing trivial, but stores time once
  per channel instead of once per index. That breaks P1's bytes-per-sample target for
  multi-channel groups (A7).
- **Index mapping only in channel metadata (Synnax today).** A Cesium data channel is
  bound to one index domain when it opens (`cesium/internal/unary/db.go:71`), and its
  samples get timestamps only through that index. The channel service offers rename but
  no index change (`core/pkg/distribution/channel/rename.go:34`), so a re-index means a
  new channel. If the index could change, old samples would pair with the new index's
  timestamps with no error, and repair could not detect it.
- **New channel key on re-index:** violates the requirement and breaks every reference
  (calculations, views, policies).

### Risks

- T is compared against each index's own clock. Poor time sync widens the gap at the
  cut; the time-sync fork's accuracy numbers bound it.
- A dashboard over the cut touches two homes until A's retention ends.
- A split or merge that commits channel by channel would cut at different T values.
  The mesh change must be atomic.

**Decision for the user:** is a re-index a committed epoch with one cut time T (the
old index stores the channel before T, the new index from T, history never rewritten),
with each stored chunk naming its index through its chunk group?

## Notes outside this fork's scope

- The named index groups with one count each (Q1, Q5) and the stateful-vs-block codec
  split (Q3) belong in the wire framing spec. The transport fork should pick them up.
- The re-index cut time T (Q5) needs the mesh to commit multi-channel changes
  atomically. The consensus fork should confirm it can.
- io_uring and O_DIRECT depend on C2's runtime choice.
