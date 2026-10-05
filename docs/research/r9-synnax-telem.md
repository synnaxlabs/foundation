# Foundation r9: Synnax telemetry and type architecture

Date: 2026-10-04. Scope: `x/go/telem` (incl. `versions/v0`, `op`, `pb`), `x/ts/src/telem`,
`x/cpp/telem`, `x/py/x/telem`, the four frame codecs, and how Cesium and the Core consume
them. Method: read the code; read RFC 0007, 0016, 0035, 0046 only for intent; ran a
scratch Go program against `x/go` and one Python calculation to verify six claims
(appendix A). All cites are `path:line` at HEAD of
`sy-4969-restructure-the-docs-site-around-capabilities-and-editions`.

## 0 Summary

Synnax's telem model is right in shape: integer-nanosecond time types, a typed byte
buffer per channel, a frame of parallel keys and series, and a stateful codec that
sends what both ends know only once. Most of its pain comes from five causes:

1. Four hand-written implementations (Go, TS, C++, Python) that drift. Oracle marks
   every telem type `@hand` outside Go (`schemas/x/telem.oracle:16-130`).
2. Open-ended string data types and a `0` density that means both "unknown" and
   "variable".
3. Per-series metadata (time range, data type, alignment) that repeats what the
   definitions already say, plus an alignment that encodes storage position.
4. Unchecked conversions: typed reads ignore the declared type, unaligned writes can
   vanish, float math in time conversions, JSON-unsafe 64-bit integers.
5. Copies and allocations on every encode and decode in every language.

Foundation's locked decisions (S1, S2, A4, A5, A8, A9, A19, S7, S13) already fix 3 and
most of 4. This report adds the type-level fixes and a concrete `types` surface.

## 1 What each type is and does

Line counts (hand-written, non-test): Go 2,942 plus 8,971 generated in `op`; TS 3,868;
C++ 3,543; Python 1,742. Four frame codecs: 2,139 lines (Go 905, TS 465, Python 385,
C++ 384).

### 1.1 TimeStamp

- Go: `type TimeStamp int64`, ns since the Unix epoch, UTC
  (`x/go/telem/versions/v0/types.gen.go:14-17`). `TimeStampMin = 0`, `Max = 2^63-1`
  (`v0/time_stamp.go:21-26`). JSON as a decimal string (`v0/time_stamp.go:33-46`).
  `String()` truncates to milliseconds and prints `MAX` as "end of time"
  (`v0/time_stamp.go:48-55`). `Add` saturates through `clamp.AddInt64`
  (`v0/time_stamp.go:75-78`). `Now()` and `Since()` read the wall clock
  (`x/go/telem/time_stamp.go:39-47`).
- TS: class over `bigint` (`x/ts/src/telem/telem.ts:78-110`). Accepts Date, ISO string,
  `[y, m, d]`, number, bigint. No argument means now (`telem.ts:88`). A `"local"` zone
  adds the UTC offset into the stored value (`telem.ts:97`). About 40 calendar getters
  and setters, 7 render formats (`telem.ts:276-560`, `types.gen.ts:14-24`). `MIN = 0`
  (`telem.ts:699-705`).
- C++: class over `int64_t` (`x/cpp/telem/telem.h:303-312`). `iso8601()` prints up to
  9 fraction digits (`telem.h:336-380`). `min()` is `INT64_MIN` (`telem.h:391-393`).
  `now()` reads `system_clock` (`telem.h:398-408`). Operators include
  `Stamp * Stamp`, `Stamp / Stamp`, `Stamp + Stamp` (`telem.h:438-472`).
- Python: `int` subclass (`x/py/x/telem/telem.py:26-64`). Converts datetime and
  timedelta through `float` seconds (`telem.py:50-56`). `now()` goes through
  `datetime.now()` (`telem.py:79-81`). `MIN = 0` (`telem.py:505-507`).

### 1.2 TimeSpan

Signed `int64` ns in every language (`v0/types.gen.go:19-22`). Go prints exact parts
"1d 2h 3m 4s 5ms 6µs 7ns" (`v0/time_span.go:55-107`), JSON as a string
(`v0/time_span.go:34-47`). TS adds a parser for the same grammar, `2h 30m`, `1.5h`,
`us` or `µs` (`telem.ts:736-754`). TS and Python set `MIN = 0` (`telem.ts:1077-1082`,
`telem.py:493-494`) although negative spans are valid in Go (`v0/time_stamp.go:88-91`).
C++ `TimeSpan` (`telem.h:46-291`).

### 1.3 TimeRange

`{start, end}`, start inclusive, end exclusive (`v0/types.gen.go:24-32`). Methods:
`Span`, `BoundBy`, `ContainsStamp`, `ContainsRange`, `OverlapsWith`, `MakeValid`
(silently swaps), `Union`, `Intersection` (returns `TimeRangeZero` when disjoint),
`Split` (`v0/time_range.go:14-171`). `String()` elides the parts the end shares with
the start and appends the span (`v0/time_range.go:94-131`). Constants `MAX`, `MIN`
(inverted), `ZERO = {0, 0}` (`v0/time_range.go:173-180`). Generated binary codec
(`v0/codec.gen.go:16-40`). TS class with delta-tolerant `equals`, `overlapsWith`
(`telem.ts:1310-1534`). Python is a pydantic model (`telem.py:625-756`). C++ plain
struct (`telem.h:575-636`).

### 1.4 Rate, Size, Density

- Rate: Hz. Go `float64`, `Period() = Second / r` (`v0/rate.go:13`). C++ `float`
  (32-bit) (`telem.h:661-676`), `period()` rounds to integer ns (`telem.h:725-727`).
  TS adds `sampleCount`, `byteCount`, `span` (`telem.ts:1117-1221`). Python `float`
  (`telem.py:541-616`).
- Size: bytes, SI units (`v0/size.go:12-30`). TS class with unit getters
  (`telem.ts:1852-2020`). Python `int` (`telem.py:814-920`). C++ has none.
- Density: bytes per sample, `uint8`, `0 = UnknownDensity`; `SampleCount` and `Size`
  panic on 0 (`v0/density.go:12-50`). String, JSON, and bytes also report 0
  (`v0/data_type.go:14-34`). C++ keeps it as `size_t density_` inside `DataType`
  (`telem.h:947`, `1028`).

### 1.5 DataType

An open-ended string in every language.

- Go: `type DataType string` (`v0/types.gen.go:42-45`), 17 constants, `""` is unknown
  (`v0/data_type.go:36-73`). `Density()` and `IsVariable()` are switches and string
  compares (`v0/data_type.go:14-34`). `InferDataType[T]` maps Go types
  (`x/go/telem/data_type.go:57-95`).
- TS: class over `string`; maps for typed-array constructor, density, short names;
  `canSafelyCastTo`; `UNKNOWN = "unknown"` (`telem.ts:1536-1849`, `1708`).
- C++: `std::string` plus a static density map and a `type_index` map
  (`telem.h:922-1224`). `cast(const void*, DataType)` reinterprets raw pointers by type
  name (`telem.h:1092-1124`). A comment warns that `inline const` globals break density
  lookups (`telem.h:1292-1293`): static-initialization order.
- Python: `str` subclass with NumPy maps; `UNKNOWN = ""` (`telem.py:923-1160`).

### 1.6 Series

- Go: `{TimeRange, DataType, Data []byte, Alignment, cachedLength *int64}`
  (`x/go/telem/series.go:33-45`). Fixed types: `Len = bytes / density`. Variable types
  (string, JSON, bytes): each sample has a `uint32` LE length prefix
  (`series_factory.go:127-155`), so `Len`, `At`, `Samples` scan
  (`series.go:48-69`, `147-200`). `Validate` checks byte length, prefix chain, UTF-8,
  JSON (`series.go:71-142`). Typed access by unsafe slice cast
  (`series.go:228-244`, `series_factory.go:157-205`). `ByteOrder` is little-endian
  (`series_factory.go:235-237`); `x/go/unsafe` builds only on LE targets
  (`x/go/unsafe/unsafe.go:10-12`). Helpers: `Downsample`, `DeepCopy`, `CopyFrom`
  (hot-path reuse, `series.go:302-357`), `NewSeriesFromAny` and `CastNumeric`
  (`series_factory.go:250-334`). Vectorized math in `op` (generated per type,
  `x/go/telem/op/op_generated.go`, 8,971 lines).
- TS: class with `dataType`, `timeRange`, `alignment`, `alignmentMultiple`,
  `sampleOffset`, a random `key`, WebGL buffer state, refcount, cached min, max,
  lengths, and offsets (`x/ts/src/telem/series.ts:149-209`). Constructor infers type
  from JS values, number -> float64, object -> JSON (`series.ts:244-384`). `alloc` and
  `write` support fill-in-place buffers (`series.ts:392-486`).
- C++: `shared_ptr<std::byte[]>` with copy-on-write keyed on `use_count()`
  (`x/cpp/telem/series.h:81-115`); copy constructor private and deep
  (`series.h:152-164`); element-wise operators (`series.h:1310-1430`).
- Python: pydantic model, `data: bytes`, NumPy via `np.frombuffer`
  (`x/py/x/telem/series.py:38-199`).

### 1.7 MultiSeries

Ordered list of same-type series for one channel. Go sorts by alignment and panics on
mixed types (`series.go:422-558`). TS adds alignment iterators and GL helpers
(`series.ts:1329-1663`). Python (`series.py:337-439`). C++ (`series.h:1805-1825`).

### 1.8 Frame

- Go: `Frame[K]` with parallel `keys []K` and `series []Series`; keys may repeat; a
  128-bit mask filters without copying for frames of up to 128 entries
  (`x/go/telem/frame.go:28-49`, `443-464`). `Get(key)` allocates a `MultiSeries`
  (`frame.go:322-331`). `Len()` allocates a map (`frame.go:364-378`). JSON and msgpack
  forms (`frame.go:270-320`). `UnsafeReinterpretKeysAs` swaps key types without a copy
  (`frame.go:51-59`).
- Core: `framer.Frame` wraps `telem.Frame[channel.Key]` and splits by leaseholder
  (`core/pkg/distribution/framer/frame/frame.go:23-76`). `channel.Key` is 12 bits of
  node plus 20 bits of local key (`core/pkg/distribution/channel/channel.go:25-49`).
- C++: `unique_ptr<vector<uint32_t>>` plus `unique_ptr<vector<Series>>`
  (`x/cpp/telem/frame.h:24-37`).
- Clients: frames keyed by key or by name (`client/ts/src/framer/frame.ts:27-152`,
  `client/py/synnax/framer/frame.py:39-69`).

### 1.9 Alignment

`uint64 = domainIndex << 32 | sampleIndex` (`x/go/telem/alignment.go:26-41`). JSON as a
string (`alignment.go:49-62`). Cesium assigns it from storage position: a writer takes
a new domain from a counter (`cesium/internal/unary/writer.go:221`), derives the sample
index from its byte tracker (`writer.go:292-307`); data channels must borrow the index
channel's cursor (`writer.go:256-272`, `cesium/internal/unary/db.go:78-83`); unpersisted
data lives in a reserved region `MaxUint32 - 1e6` (`cesium/internal/alignment/alignment.go:18-27`).
TS adds `alignmentMultiple` for decimated views (`series.ts:181-187`).

### 1.10 Other types

- C++ `SampleValue` variant (no bool, no UUID) and casts (`telem.h:785-920`); Python
  `SampleValue` (`series.py:301-303`); TS `TelemValue` (`telem.ts:2108-2122`).
- `MonoClock`: forces strictly increasing stamps by adding 1 ns
  (`x/go/telem/mono_clock.go:12-44`); C++ twin (`x/cpp/telem/mono_clock.h`).
- Clock skew estimation lives in telem (`x/ts/src/telem/clockSkew.ts`,
  `x/py/x/telem/clock_skew.py`, `x/cpp/telem/clock_skew.h`).

### 1.11 Wire form (frame codec)

Stateful: both ends register `(keys, data types)` under a sequence number; each frame
carries 1 flag byte + 4-byte state number, then length, time range, alignment either
once (uniform) or per series, plus per-series keys when not all channels are present
(`core/pkg/distribution/framer/codec/codec.go:328-387`, `576-763`). Contiguous series
of one key merge on encode (`codec.go:427-548`). Booleans are bit-packed on the wire
only (`codec.go:55-82`; RFC 0046 §3.0). This is RFC 0016, written in 2023 to replace
JSON frames.

### 1.12 How Cesium and the Core consume them

- Cesium writes `Series.Data` bytes to its files unchanged
  (`cesium/internal/unary/writer.go:308-312`): memory format = disk format already.
- Cesium checks the series type against the channel type, with an int64 = timestamp
  exception (`cesium/internal/channel/channel.go:95-110`). The same check exists in the
  codec (`codec.go:598-608`) and the Core writer validator
  (`core/pkg/service/framer/writer/validator.go:79-92`).
- Index channels own the alignment space; a write must carry every data channel of an
  index (`cesium/writer_stream.go:819-845`).
- The Core relay filters each frame per streamer with `KeepKeys`
  (`core/pkg/distribution/framer/relay/streamer.go:175`), which is why the mask exists.
- Storage and distribution key types are swapped by unsafe reinterpretation
  (`core/pkg/distribution/framer/frame/frame.go:83-85`, `122-125`).

## 2 What Foundation's `types` crate should keep

1. **Integer-nanosecond time with three distinct types.** Stamp, span, and range as
   separate types made unit errors rare. C++ got the arithmetic right:
   `Stamp - Stamp = Span` (`telem.h:430-432`). Half-open ranges kept boundary handling
   predictable (`v0/types.gen.go:24-32`). A5 and A9 already lock i64 ns.
2. **One rich format and parse grammar.** The span parts format
   (`v0/time_span.go:55-107`), the span parser (`telem.ts:736-754`), the range elision
   (`v0/time_range.go:94-131`), and nanosecond ISO output (`telem.h:336-380`) are what
   people read in logs and type in config. Foundation's config already needs
   `"3d"`, `"10kHz"`, `"50GB"` (S12, C3). Keep one grammar, implemented once.
3. **Width per type, count = bytes / width.** Density made fixed-width series O(1) to
   measure and index (`v0/density.go:15-35`). S2 depends on it.
4. **A series is typed bytes in one contiguous little-endian buffer.** Zero-copy typed
   views worked in Go (`unsafe.go:79-83`), Python (`series.py:186`), and TS typed
   arrays. This is what makes A9's NumPy zero-copy possible.
5. **Memory format = disk format.** Cesium writes series bytes as received
   (`unary/writer.go:308-312`). S2 extends this to the wire.
6. **Frame as parallel key and series arrays, series shared by reference.** Go slices
   and C++ `shallow_copy` (`frame.h:124-126`) let many readers share one buffer.
7. **A stateful codec that sends known facts once.** Per-connection key and type
   tables with a version number for the switch-over window (`codec.go:135-174`,
   `765-803`), and flags that elide uniform metadata (`codec.go:328-387`). This is
   the prototype of S1, S2, and A4's per-connection short numbers.
8. **A monotone per-channel position for matching samples across channels.**
   Alignment's purpose was right; A8's `seq` keeps the purpose and drops the storage
   coupling.
9. **Validation at ingest.** `Series.Validate` (`series.go:71-142`, SY-4085, commit
   8f79f9f3f4) and bool normalization to `0x00`/`0x01` (RFC 0046 §3.5). Keep the
   checks; run them once (BQ4).
10. **Buffer reuse.** `CopyFrom` for hot paths (`series.go:350-357`), `AllocFrame` and
    `Grow` (`frame.go:75-81`, `341-349`), reusable codec buffers
    (`codec.go:139-151`). The instinct was right; Foundation makes pooling the default.
11. **Readable debug output.** Series print type, length, size, truncated values, and
    timestamp deltas (`series.go:285-409`).
12. **Test helpers and fuzzed codecs.** Gomega matchers (`x/go/telem/matchers.go`) and
    fuzzed generated codecs (SY-4080). Foundation: proptest strategies exported from
    `types` and a fuzz target per decoder (T1).

## 3 What Foundation should fix or drop

Each item: the problem in Synnax, then the fix.

1. **Four hand-written copies drift.** Oracle generates only Go aliases; every other
   language is `@hand` (`schemas/x/telem.oracle:16-130`). Observed drift:
   - unknown type: `""` in Go, Python, C++; `"unknown"` in TS (`telem.ts:1708`,
     `telem.py:1065`);
   - minimum stamp: 0 in Go, TS, Python; `INT64_MIN` in C++ (`telem.h:391-393`);
   - type inferred from a plain number: TS float64 (`series.ts:292`), Python int64
     for `int` (`telem.py:944-945`);
   - unknown codec state number: Go errors (`codec.go:792-802`), TS and Python return
     an empty frame silently (`client/ts/src/framer/codec.ts:283-284`,
     `client/py/synnax/framer/codec.py:267-269`), C++ throws `std::out_of_range`
     (`client/cpp/framer/codec.cpp:320`);
   - old codec states: Go and TS never prune (`codec.go:313-326`), Python prunes
     (`codec.py:271-280`);
   - wire time ranges: Go writes `uint64` (`codec.go:84-87`), C++ reads `int64`
     (`codec.cpp:325-326`), TS and Python read unsigned;
   - stamp text: Go milliseconds, C++ nanoseconds, Python microseconds;
   - overflow: Go saturates (`v0/time_stamp.go:76-78`), C++ signed overflow is
     undefined behavior (`telem.h:426-428`).

   Fix: one Rust implementation. The Rust SDK reuses it (C1); the Python SDK binds it
   (decision D12).
2. **String data types.** Open set: any string constructs a type in every language.
   One string per series on the gRPC wire (`x/go/telem/pb/frame.proto:22-27`). String
   compares in hot paths (`telem.h:1054-1124`, `telem.ts:1591-1597`). A rename of
   `"bool"` to `"boolean"` touched wire, storage, and four languages (SY-4596, commit
   7480a57260). C++ static-init hazard (`telem.h:1292-1293`). Fix: closed
   `#[repr(u8)]` enum with stable discriminants; names only for text.
3. **Density 0 means two things.** "Unknown" and "variable" share 0; `SampleCount`
   panics on it (`v0/density.go:19-22`). Fix: `width() -> Option<NonZero>`.
4. **Per-sample length prefixes.** `Len`, `At`, and iteration scan from the start in
   all four languages (`series.go:48-69`, `176-200`; `series.ts:598-617`, `868-905`;
   `series.py:68-82`, `221-234`; C++ `series.h:733-740`). Cesium keeps an in-memory
   offset table rebuilt by a scan after restart (RFC 0035 §0). The format before
   that, newline-separated, corrupted samples that held a newline (SY-4059, commit
   2865e72aa7, 23 files across 4 languages). Fix: offsets array (R2 `ends[n]`).
5. **Lost and stale length cache in Go.** `Len()` has a value receiver, so a computed
   length is cached on a copy and lost (`series.go:48-66`). `CopyFrom` keeps a stale
   cached length: verified `Len() = 3` for a one-sample series (appendix A).
   Fix: immutable series; count stored, never cached lazily.
6. **JSON as a data type.** Per-sample JSON validation on the write path
   (`series.go:116-121`); TS rewrites JSON keys from snake to camel case on read
   (`series.ts:806`); TS and Python infer JSON for any object or dict
   (`series.ts:301`, `telem.py:972-978`). A19 already drops it.
7. **64-bit integers in JSON.** Each type opted in to string encoding by hand
   (`alignment.go:49-62`, `v0/time_stamp.go:33-46`, `v0/time_span.go:34-47`). The
   `Size` string encoder was added (commit 8874635df0) and later dropped in an unrelated
   refactor (commit 10ab96b639); nothing caught it. TS parses the string form back into
   a float `Number` (`telem.ts:2060-2063`) and ships lossy `NumericTimeRange` and
   `NumericTimeSpan` (`telem.ts:2049-2069`). Python turns alignment into `float`
   (`series.py:275-278`). C++ sends stamps to protobuf as `double`
   (`telem.h:913-914`). Plots quantized values above 2^53 until SY-4149 (commit
   f439caac5d). Fix: JSON forms defined once per type (D7); integers above 2^53 never
   leave as JSON numbers.
8. **Unchecked and unsafe conversions.**
   - `Unmarshal[T]` and `ValueAt[T]` ignore the declared type
     (`series_factory.go:165-166`, `series.go:228-235`); float64 bytes read as int64
     return garbage (verified).
   - `CastSlice` silently copies an unaligned buffer (`unsafe.go:79-88`), so
     `SetValueAt` writes into a temporary and the write is lost
     (`series.go:240-244`). Verified: the bytes stay zero when the buffer address is
     1 mod 8; the outcome depends on where the buffer lands.
   - `CastNumeric` truncates and wraps by design (`series_factory.go:292-295`).
   - int64 accepted for timestamp channels in three places (`codec.go:598-600`,
     `validator.go:82-84`, `channel.go:98-100`).
   - C++ maps protobuf null, struct, and list to `0.0` (`telem.h:884-903`).
   - Python drops non-UUID items from a UUID list (`series.py:138-140`).

   Fix: typed views checked against the series type; aligned buffers so a view never
   copies; lossy conversions named (`checked_`, `saturating_`, `as_f64`).
9. **Floats in time math.** Python converts datetime through float seconds
   (`telem.py:50-56`): a microsecond datetime becomes a stamp 24 ns off (appendix A).
   C++ stores rates as 32-bit floats (`telem.h:661-676`) and rounds the period to whole
   ns (`telem.h:725-727`): at 48 kHz that is 1.38 s per day if accumulated. The
   hardware sample clock hides it with a PID loop against the PC clock
   (`driver/common/sample_clock.h:181-215`). Fix: integer-only conversions; exact
   rational rates (D8).
10. **Ambient clock and local time inside values.** `Now()`, `Since()`
    (`time_stamp.go:39-47`); `MonoClock` falls back to the wall clock when no source is
    set (`mono_clock.go:33-37`) and fakes 1 ns steps (`mono_clock.go:38-41`, A5 rejects
    this); TS `new TimeStamp()` is now and `"local"` shifts the stored value
    (`telem.ts:88`, `97`); Python treats naive datetimes as local (`telem.py:33-36`).
    Fix: no clock reads in `types` (C1, T1); values are UTC; zones only at formatting.
11. **Weak time arithmetic.** Go `TimeStamp - TimeStamp` is a `TimeStamp` (both are
    `int64` types); Python `TimeStamp + TimeStamp` is a `TimeStamp`
    (`telem.py:155-159`); C++ allows `Stamp * Stamp` (`telem.h:438-472`). Fix: Rust
    operator impls only for meaningful pairs.
12. **Alignment as storage position.** A magic reserved region
    (`cesium/internal/alignment/alignment.go:18-27`). The sample index wraps inside its
    domain and goes backwards: `7-4294967295` + 2 = `7-1` (`alignment.go:64-67`,
    verified). Python ORs the overflow into the domain bits (`telem.py:1222-1224`, `1264-1270`). The
    `MultiSeries` sort comparator `int(a - b)` on `uint64` puts a reserved-region series
    before a persisted one (`series.go:425-427`, verified). The bug class recurs:
    commits d9d1e5fbbc (2024-06, index and data channels with different domain counts),
    3a476fae38 (2025-08, calculated channels across domains), c1f4da76e5 (2026-03,
    writer alignment race), 04a6bbff9a (2026-07, "single alignment authority", 1,255
    lines). Fix: A8 `seq`, u64 per index, checked increments.
13. **Per-series metadata that repeats definitions.** Every series carries a time range,
    a data type, and an alignment (`series.go:33-45`). Non-uniform frames pay 32 bytes
    per series on the wire (key 4, length 4, time range 16, alignment 8;
    `codec.go:700-719`). S1 and S2 already fix this: time is the index series, type
    comes from the definition.
14. **Repeated keys in a frame.** Keys may repeat (`frame.go:31-33`), so `Get` builds a
    `MultiSeries` (`frame.go:322-331`) and the encoder sorts and merges runs with an
    allocation and a copy (`codec.go:427-548`). Go's value-receiver `Append`
    (`frame.go:335-339`) can let two frames share one backing slot. Fix: S1's one
    series per key, checked once at the home.
15. **Copies and allocations on every hop.** Go `Encode` copies the whole output
    (`codec.go:396-399`) and decode allocates per series (`codec.go:836-846`); TS copies
    per series (`codec.ts:321`); Python copies per series (`codec.py:329`); C++ copies
    the whole codec state per encode and decode (`codec.cpp:127`, `320`) and allocates
    per series; bool pack and unpack allocate (`codec.go:62-82`). C++ frames need four
    heap allocations (`frame.h:35-37`). TS mints a random id per series
    (`series.ts:254`). Fix: decode into pooled blocks, encode into a caller's pooled
    buffer, no allocation per frame at steady state (S2).
16. **Copy-on-write keyed on `use_count()`.** C++ decides to copy by
    `use_count() > 1` (`series.h:106-115`), which the standard calls approximate under
    concurrency, and `const` methods mutate through `mutable data_`
    (`series.h:97`, `183-190`). Fix: a writable unique block that freezes into a shared
    immutable block; no copy-on-write.
17. **Rendering and derived state inside the value type.** TS `Series` holds a WebGL
    buffer, refcount, cached bounds, a sample offset (`series.ts:152-209`). Fix: the
    value holds data only; caches live with consumers.
18. **Panics on outside input.** Density 0 (`v0/density.go:19-22`), mixed types in
    `NewMultiSeries` (`series.go:438-444`), out-of-range `At` (`series.go:192-196`),
    codec not updated (`codec.go:402-411`). Fix: `Result` for anything that came from
    outside; panics only for programmer errors.
19. **Math inside the value type.** 8,971 generated lines of per-type kernels
    (`op_generated.go`), `Downsample` (`series.go:302-332`), C++ operators
    (`series.h:1310-1430`). Fix: `types` holds no math; the `calc` crate (A17) owns
    kernels, using generics instead of code generation.
20. **Placement baked into the key.** `channel.Key` holds the node in its top 12 bits
    (`channel.go:25-49`), so a channel cannot change home and a mesh caps at 4,096
    nodes. A4 (UUIDv7) and A1 (home separate) already fix this.
21. **The same check in three layers.** The data-type check with the int64 exception
    is copied in the codec, the Core validator, and Cesium (item 8). Fix: validate once
    at the home (BQ4, D14).
22. **Three bool formats.** Byte in memory, bit on the wire, byte on disk (RFC 0046
    §3.0, `codec.go:55-82`). This conflicts with S2's one format. See D2.

## 4 Proposed public surface for `types`

Rules applied: values only, no I/O, no clock reads, no randomness, no globals (C1, T1);
the module carries the context (`time::Stamp`, `series::Series` as the blessed
exception); no copies on the hot path; no boolean fields. `spec` holds the meaning
(enum names, flags, units, quality semantics); `types` holds the bytes. The `name`
module (A3 names, S12 selectors, BQ2) is listed but not designed here.

```rust
//! crate `types`: values shared by every Foundation crate and the Rust SDK.

#[cfg(not(target_endian = "little"))]
compile_error!("Foundation supports little-endian targets only");

pub use error::Error;          // one error enum; every variant has a stable code (C7)
pub use frame::Frame;
pub use series::Series;

pub mod time {
    /// Nanoseconds since the Unix epoch, UTC (A5).
    #[repr(transparent)]
    #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub struct Stamp(i64);

    impl Stamp {
        pub const EPOCH: Stamp;
        pub const MIN: Stamp;
        pub const MAX: Stamp;
        pub const fn from_nanos(ns: i64) -> Stamp;
        pub const fn nanos(self) -> i64;
        pub fn checked_add(self, span: Span) -> Option<Stamp>;
        pub fn checked_sub(self, span: Span) -> Option<Stamp>;
        /// Start of the `unit` window that holds `self`. `unit` must be positive.
        pub fn floor(self, unit: Span) -> Stamp;
    }
    // Ops: Stamp - Stamp = Span; Stamp + Span, Stamp - Span = Stamp. Overflow panics
    // (#[track_caller]). No Stamp + Stamp, no Stamp * n.
    // Text: RFC 3339 UTC, 9 fraction digits. FromStr accepts any offset, rejects a
    // missing offset. TryFrom<std::time::SystemTime> is a pure conversion.

    /// Signed nanosecond duration (the A9 `duration` type).
    #[repr(transparent)]
    #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub struct Span(i64);

    impl Span {
        pub const ZERO: Span;
        pub const NANOSECOND: Span;
        pub const MICROSECOND: Span;
        pub const MILLISECOND: Span;
        pub const SECOND: Span;
        pub const MINUTE: Span;
        pub const HOUR: Span;
        pub const DAY: Span;
        pub const fn from_nanos(ns: i64) -> Span;
        pub const fn nanos(self) -> i64;
        pub fn checked_mul(self, n: i64) -> Option<Span>;
        pub fn abs(self) -> Span;
        /// Lossy by name.
        pub fn as_secs_f64(self) -> f64;
    }
    // Ops: Add, Sub, Neg, Mul<i64>, Div<i64>, Rem. Overflow panics.
    // Text: "1d 2h 3m 4s 5ms 6us 7ns"; FromStr also takes "1.5s", "250us". Exact,
    // ASCII only. TryFrom in both directions with std::time::Duration.

    /// Half-open [start, end). Invariant: start <= end.
    #[derive(Copy, Clone, PartialEq, Eq, Hash)]
    pub struct Range { start: Stamp, end: Stamp }

    impl Range {
        pub fn new(start: Stamp, end: Stamp) -> Result<Range, crate::Error>;
        pub fn start(self) -> Stamp;
        pub fn end(self) -> Stamp;
        pub fn span(self) -> Span;
        pub fn contains(self, at: Stamp) -> bool;
        pub fn intersect(self, other: Range) -> Option<Range>;
        pub fn hull(self, other: Range) -> Range;
    }
    // Text: start, then the parts of end that differ, then the span.
}

/// Exact rate in hertz, a reduced fraction num / den.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Rate { num: u64, den: u64 }

impl Rate {
    pub fn hz(hz: u64) -> Result<Rate, Error>;              // zero is an error
    pub fn from_period(period: time::Span) -> Result<Rate, Error>;
    /// Exact time of sample `n` after sample 0, rounded once. Never drifts.
    pub fn offset(self, n: u64) -> time::Span;
    /// Rounded to whole ns. For display; never accumulate it.
    pub fn period(self) -> time::Span;
}
// Text: "10kHz", "33.3333Hz", "1MHz".

/// A byte count. FromStr takes SI and IEC units ("50GB", "64MiB"); Display uses SI.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Size(u64);

/// Sample sequence number of an index (A8).
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Seq(u64);

impl Seq {
    pub const fn new(n: u64) -> Seq;
    pub const fn get(self) -> u64;
    /// Panics on overflow: unreachable by A8's arithmetic, so overflow is a bug.
    pub fn advance(self, n: u64) -> Seq;
}

pub mod channel {
    /// UUIDv7 (A4). Built from parts by the caller that owns the clock and RNG.
    #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Key([u8; 16]);

    impl Key {
        pub fn from_bytes(bytes: [u8; 16]) -> Result<Key, crate::Error>; // checks v7
        pub fn from_parts(unix_ms: u64, random: u128) -> Key;
        pub const fn bytes(self) -> [u8; 16];
    }

    /// BuildHasher that reads the random tail of the UUIDv7: no hashing work.
    pub struct Hasher;
}

pub mod node {
    /// UUIDv7 (S8). Same layout and rules as channel::Key.
    #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Key([u8; 16]);
}

pub mod sample {
    use std::num::{NonZeroU32, NonZeroU8};

    /// Byte layout of one value. Discriminants are part of the wire and disk format
    /// and never change.
    #[repr(u8)]
    #[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
    pub enum Primitive {
        Bool = 1, I8 = 2, I16 = 3, I32 = 4, I64 = 5,
        U8 = 6, U16 = 7, U32 = 8, U64 = 9, F32 = 10, F64 = 11,
        Timestamp = 12, Duration = 13, Uuid = 14, String = 15, Bytes = 16,
    }

    impl Primitive {
        /// None for String and Bytes.
        pub const fn width(self) -> Option<NonZeroU8>;
        pub const fn name(self) -> &'static str;
    }

    /// Layout of a channel's samples. Enums and flags are their integer layout,
    /// quality is U32; names and meaning live in `spec` (A11, A12, S13).
    #[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
    pub enum Type {
        Primitive(Primitive),
        /// T[N] and T[N][M], row-major, flattened; the shape lives in `spec`.
        Array { element: Primitive, len: NonZeroU32 },
        /// list<T, max> (A13).
        List { element: Primitive, max: NonZeroU32 },
    }

    impl Type {
        /// Bytes per sample. None for variable layouts.
        pub const fn width(self) -> Option<NonZeroU32>;
    }

    /// A Rust type whose bytes are one value of `PRIMITIVE`.
    /// Implemented for i8..i64, u8..u64, f32, f64, time::Stamp, time::Span, [u8; 16].
    pub unsafe trait Native: Copy + 'static {
        const PRIMITIVE: Primitive;
    }
}

pub mod block {
    /// Preallocated, fixed size classes, 64-byte aligned. One per shard (C2).
    pub struct Pool { /* ... */ }

    impl Pool {
        pub fn new(config: Config) -> Pool;
        /// Fails when the pool is exhausted; never falls back to the heap.
        pub fn take(&self, len: usize) -> Result<Unique, crate::Error>;
    }

    /// A block with one owner. Writable.
    pub struct Unique { /* ... */ }

    impl Unique {
        pub fn as_mut(&mut self) -> &mut [u8];
        pub fn truncate(&mut self, len: usize);
        pub fn freeze(self) -> Block;
    }

    /// A shared, immutable block. Clone is one atomic increment; the last drop
    /// returns the memory to its pool. Deref<Target = [u8]>.
    #[derive(Clone)]
    pub struct Block { /* ... */ }

    impl Block {
        /// Zero copy.
        pub fn slice(&self, range: std::ops::Range<usize>) -> Block;
    }
}

pub mod series {
    use crate::{block, sample, Error, Seq};

    /// Samples of one channel. The wire and the disk carry only `bytes()` (S2);
    /// `ty` and `len` exist in memory only (D1).
    #[derive(Clone)]
    pub struct Series { seq: Seq, ty: sample::Type, len: u32, data: block::Block }

    impl Series {
        /// O(1) structural check: width * len == bytes, or last offset == bytes.
        pub fn new(
            seq: Seq,
            ty: sample::Type,
            len: u32,
            data: block::Block,
        ) -> Result<Series, Error>;
        /// Full content check (offsets, UTF-8, bool bytes). Run once, at the home.
        pub fn check(&self) -> Result<(), Error>;
        pub fn seq(&self) -> Seq;
        pub fn ty(&self) -> sample::Type;
        pub fn len(&self) -> u32;
        pub fn is_empty(&self) -> bool;
        pub fn bytes(&self) -> &[u8];
        pub fn block(&self) -> &block::Block;
        /// Zero copy. Errors on a type mismatch or misalignment; never copies.
        pub fn values<T: sample::Native>(&self) -> Result<&[T], Error>;
        pub fn bools(&self) -> Result<&[bool], Error>;
        pub fn strings(&self) -> Result<Strings<'_>, Error>;
        pub fn byte_strings(&self) -> Result<ByteStrings<'_>, Error>;
        /// Zero copy for fixed widths; seq advances by `range.start`.
        pub fn slice(&self, range: std::ops::Range<u32>) -> Result<Series, Error>;
    }

    /// Writes fixed-width values into a pooled block.
    pub struct Builder<T: sample::Native> { /* ... */ }

    impl<T: sample::Native> Builder<T> {
        pub fn new(block: block::Unique) -> Builder<T>;
        pub fn push(&mut self, value: T) -> Result<(), Error>;
        pub fn extend(&mut self, values: &[T]) -> Result<(), Error>;
        pub fn finish(self, seq: Seq) -> Series;
    }

    /// O(1) access to string samples through the offsets array.
    pub struct Strings<'a> { /* offsets: &'a [u32], data: &'a [u8] */ }

    impl<'a> Strings<'a> {
        pub fn get(&self, i: u32) -> Option<&'a str>;
        pub fn iter(&self) -> impl Iterator<Item = &'a str>;
    }
    // ByteStrings, Lists<'a, T>, and the string and list builders follow the same shape.
}

pub mod frame {
    use crate::{channel, Error, Series};

    /// Series for several channels, one series per key (S1).
    #[derive(Clone, Default)]
    pub struct Frame { keys: Vec<channel::Key>, series: Vec<Series> }

    impl Frame {
        pub fn with_capacity(n: usize) -> Frame;
        pub fn push(&mut self, key: channel::Key, series: Series);
        /// Linear scan; frames are small.
        pub fn get(&self, key: channel::Key) -> Option<&Series>;
        pub fn keys(&self) -> &[channel::Key];
        pub fn series(&self) -> &[Series];
        pub fn iter(&self) -> impl Iterator<Item = (channel::Key, &Series)>;
        /// In place, no allocation.
        pub fn retain(&mut self, keep: impl FnMut(channel::Key) -> bool);
        /// Keeps capacity for reuse.
        pub fn clear(&mut self);
        /// One series per key. Run once, at the home.
        pub fn check(&self) -> Result<(), Error>;
    }
}

pub mod quality {
    /// OPC UA status code (S13).
    #[repr(transparent)]
    #[derive(Copy, Clone, PartialEq, Eq, Hash)]
    pub struct Code(u32);

    pub enum Severity { Good, Uncertain, Bad }

    impl Code {
        pub const GOOD: Code;
        pub const fn new(raw: u32) -> Code;
        pub const fn raw(self) -> u32;
        pub const fn severity(self) -> Severity;
    }
}

pub mod name { /* Name, Pattern, Selector (A3, S12, C8 exclusions); not in this report */ }

// Behind a `testing` feature: proptest strategies for every type (T1 layers 1, 2).
```

Notes on the surface:

- Serde: `Serialize`/`Deserialize` for every value type with the D7 forms. `Series` and
  `Frame` have no JSON form; only the codec moves them.
- The Pod-cast dependency (bytemuck or zerocopy) is a choice for the R7 audit. Either
  gives checked `&[u8] -> &[T]` casts that fail instead of copying.
- `bools()` scans for bytes above 1 on each call, because `&[bool]` over any other byte
  is undefined behavior in Rust. The scan is a SIMD pass. Callers that need speed use
  `values::<u8>()` and test for nonzero.
- `Frame::get` is linear. A home with large frames keeps its own position map per
  index group.

## 5 Decisions for the user

**D1. Series header in memory.** S2 locks `Series { seq, data }`. Add `ty` and `len` in
memory only; the wire still carries only data bytes. Recommendation: yes. Typed views
can then check the type at the point of use (Synnax's unchecked `Unmarshal` returned
garbage), variable series know their count without a scan, and debug output works. Cost:
about 16 bytes per series in memory.

**D2. Bool layout.** One byte per sample (`0x00`/`0x01`) in the raw format everywhere,
and bit packing only as a codec compression choice (the adaptive codec already picks
schemes per block). Recommendation: yes. It keeps S2's one format and NumPy zero-copy
(`np.bool_` is one byte); Synnax needed three formats (RFC 0046 §3.0).

**D3. Raw series alignment.** Pad each raw series in the wire and disk layout to its
element width (at most 7 bytes; computed from known widths, so it carries no
information); pool blocks are 64-byte aligned. Recommendation: yes. Decode then forms
`&[f64]` in place. Synnax TS copied every series on decode partly for this reason
(`codec.ts:321`), and Go's fallback copy lost writes.

**D4. Variable-length layout.** Use R2's `ends[n]` offsets plus D1's `len`; never
per-sample prefixes. Recommendation: yes. Synnax's prefixes made every `At` a scan in
four languages. (If D1 is rejected, use n+1 offsets so the first offset gives the
count.)

**D5. Type vocabulary.** `sample::Primitive` and `sample::Type` describe byte layout
only; enums, flags, quality, and units are meaning held in `spec`.
Recommendation: yes. `types` stays small, the home validates layout without the spec
tree, and A11's "unknown values pass through" needs no type-level support.

**D6. Duration naming.** Rust value `time::Span`; config and type keyword `duration`
(A9); enum variant `sample::Primitive::Duration`. Recommendation: yes. `Span` avoids a
clash with `std::time::Duration` (unsigned, used by Tokio) in the same file; users keep
the common word.

**D7. JSON forms.** Stamp: RFC 3339 UTC with exactly 9 fraction digits (exact, readable
by agents, lexical order = time order). Span: the unit grammar string. Rate, Size: unit
strings in config, numbers of Hz or bytes in `--json` output. Seq: number (under 2^53 by
A8). Keys: UUID strings. Frames: no JSON on the data path. Recommendation: yes. Synnax's
decimal-string stamps were exact but opaque, and the per-type opt-in regressed silently.

**D8. Rate representation.** Exact reduced fraction with `offset(n)` computed in u128;
`period()` only for display. Recommendation: yes. It removes float drift (1.38 s per day
at 48 kHz with a rounded period) without a correction loop, and represents DAQmx
timebase rates (100 MHz / N) exactly.

**D9. Block ownership.** Atomic refcount; `Unique` is writable, `Block` is immutable
after `freeze`; no copy-on-write. Recommendation: yes. Frames cross shards and fan out to
readers, so blocks must be `Send`; one atomic increment per reader is small next to the
payload. C++ copy-on-write on `use_count()` was racy.

**D10. Overflow policy.** Arithmetic operators panic on overflow; `checked_*` for values
from outside; no silent saturation or wrapping. Recommendation: yes. Go saturated, C++
had undefined behavior, alignment wrapped backwards; in-range stamps (1970 to 2262)
cannot overflow, so a panic marks a bug.

**D11. Frame key type.** Keep `channel::Key` (16 bytes) in in-memory frames, hashed by
reading the UUIDv7 random tail; no node-local slot numbers until a benchmark shows key
lookup in the top costs. Recommendation: yes. One key type across layers removes
Synnax's unsafe key reinterpretation (`frame.go:83-85`). Keys are minted by the mesh, so
the identity hash is not exposed to crafted keys.

**D12. Python SDK data path.** Bind Rust `types` and `codec` into the Python package
(PyO3), with NumPy arrays as views over Rust blocks; the hand-written native layer (C7)
sits on top. Recommendation: yes. This narrows C7's "data path hand-written per
language" to the API layer. Synnax's four codecs disagreed on unknown states, state
pruning, and signedness.

**D13. Crate and module names.** Keep the crate `types` (Q21) with modules `time`,
`sample`, `series`, `frame`, `block`, `channel`, `node`, `quality`, `name`. Rename the
layer-2 `time` crate to `clock`. Recommendation: yes. `types::time::Stamp` next to a
crate called `time` reads ambiguously, and `clock` names what C6 builds (the mesh
clock).

**D14. Validation placement.** `Series::check` and `Frame::check` run once, at the home,
for frames from outside; constructors do only O(1) structural checks; no other crate
re-checks. Recommendation: yes. This is BQ4's "validate at the home". Synnax ran the
same type check in three layers and the full check in two.

## Appendix A: verification runs

Scratch module in this session's scratchpad (`r9go/`), `replace` to the local `x/go`:

| Check | Result |
| --- | --- |
| `Validate()` then `CopyFrom(one-sample series)` | `Len() = 3`, real samples 1 |
| `NewSeriesV[float64](1.5).Unmarshal[int64]()` | `4609434218613702656` |
| `NewAlignment(7, MaxUint32).AddSamples(2)` | `7-4294967295 -> 7-1` |
| `SetValueAt[float64]` on `buf[1:17]`, address 1 mod 8 | bytes stay `[0 0 0 0 0 0 0 0]` |
| `TimeStamp(1791115200123456789).String()` | `2026-10-04T12:00:00.123Z` |
| `NewMultiSeriesV(live, persisted)` order | reserved-region domain first, domain 5 second |

Python, same formula as `telem.py:53-54`: datetime `2026-10-04T12:00:00.123457Z` gives
`...123457024` ns, 24 ns off. A 48 kHz period rounded to whole ns drifts 1.3824 s per
day.
