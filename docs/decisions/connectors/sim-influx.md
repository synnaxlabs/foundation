- **SIM INFLUX (#1151)** `connector_influx::sim::Store` is a simulated InfluxDB, behind
  the cargo feature `sim`, off by default. It parses with `influxdb-line-protocol`,
  InfluxData's own parser, so it is independent of our writer. A point is named by its
  measurement, tag set, and time; a later write of the same point replaces the fields
  that it sets. It refuses a line that a writer must never write: a line that does not
  parse; no time, or a time outside `i64::MIN + 2 ..= i64::MAX - 1`; the key `time`, or
  a name or key that starts with `_`; a key more than once in tags and fields together;
  a float that parses to infinity; a `u` integer, as InfluxDB 1 OSS does; and a tag or
  field whose type differs from the type stored for that key in the measurement, where a
  tag is a type, as InfluxDB 3 gives each column one type, also across shards, where
  InfluxDB 1 checks each shard only. Each refusal is a typed `sim::Error` variant.
  `write` stores each valid line, also after a line that is not valid, and returns the
  first error; a body that is not UTF-8 gives `Error::Utf8`, and nothing is stored.
  Where InfluxDB versions differ in a rule that it keeps, the store keeps the strictest
  one. It keeps no size limit (the series key length, 65535 bytes in InfluxDB 1 and 2,
  and the columns per table in InfluxDB 3), because each depends on the server's version
  or config, and the writer owns them (#1265). The InfluxDB 3 parser also refuses some
  lines that InfluxDB 1 stores, such as a tab in a measurement name; the writer refuses
  them too. It splits lines, and skips blank lines and comments, as InfluxDB 3 does, so
  a writer that writes a measurement name with a leading `#` loses that line with no
  error; a test that reads the points sees the loss. Lost: a store that gives a time to
  a line with none, and one that takes a type conflict, as each hides a writer bug; and
  a test that a line is refused if and only if the writer refuses its input, as the
  writer also refuses some names that InfluxDB stores, such as a backslash or NUL, so
  the two sets differ by design. Decided by the architect (`laptop.architect-2`), #1151
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032723969), and in
  the review of #1239
  (https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6032923332,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6032970676,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033050251,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033093344,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033140752,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033409699,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033688614). The `u`
  refusal and the removal of `Field::Unsigned` and `Kind::Unsigned`: decided by
  `laptop.architect-2` on 2026-10-08 (2026-10-08T08:21:48Z:
  https://github.com/synnaxlabs/foundation/issues/1210#issuecomment-6055802089;
  2026-10-08T08:26:38Z:
  https://github.com/synnaxlabs/foundation/issues/1210#issuecomment-6055875543), which
  supersedes the `u` answer of
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6032970676.
  Memory: each series keeps its points in chunks, one time column and one typed
  column for each field key, so the STORE AND FORWARD scenario holds about 6e7 points
  on a CI runner (#1149). `Point::fields` is a `Fields` view of the chunk.
  `tests/memory.rs` counts the heap bytes with `counting` and asserts at most 32 a
  point after 1e6 points of the lab's line. Lost: runs of points on a fixed time step,
  as mesh slew moves each time off any grid (MESH SLEW); and the resident set size
  (RSS) in place of a byte count, as RSS depends on the allocator and the OS. Decided
  by the architect (`laptop.architect-2`) on 2026-10-07T14:23:09Z, #1419
  (https://github.com/synnaxlabs/foundation/issues/1419#issuecomment-6040009661).
  Implementation, not a ruling: a chunk holds at most 4096 points. A column holds only
  the points that set its key, each as an index and a value. A point past the end of a
  full chunk goes into the next chunk when it has room, so appends in either time order
  fill each chunk. A full column grows by an eighth, not by double, and a split frees
  the spare room of both halves. A point with one float field takes about 19 heap
  bytes in a long series. Each series also has a fixed cost of about 1.6 KB, so 1000
  series of 200 points take about 27 bytes a point. Each chunk keeps a column for each
  key it holds, in a `Vec` sorted by key, so many sparse keys cost more: 255 keys, each
  set by every 255th point, take about 24 bytes a point, and about 29 when writes split
  each chunk into two halves near half full, as each half keeps a copy of each column.
  `tests/memory.rs` bounds 22 a point for one field, for 63 sparse keys, for appends
  newest first, for writes that split chunks, also with 63 and 65 sparse keys, and for
  one point of 255 fields among points of one field, and 32 for 255 sparse keys, for
  257 and 255 sparse keys with writes that split chunks, and for 1000 series of 200
  points.
  `Fields` is read only through `iter`: `Fields::get` went, as no caller reads one
  field by key (#1579). It supersedes the `Fields::get` item of
  https://github.com/synnaxlabs/foundation/issues/1419#issuecomment-6040009661.
  Writes of a backlog newest first are 2.9 to 3.2 times slower, and `Fields::iter`
  with 63 sparse keys 10.5 to 12 times slower, than at `ae0fd3fc` (box2, Xeon 8488C,
  busy host). The architect accepts this for about 19 B a point in place of 700 to
  1388 B, as the store is in no node binary. `benches/sim.rs` times the store.
  `cargo bench -p connector-influx` turns on `sim` through a dev-dependency of the
  crate on itself, since the bench host runs no features. Until its baseline on a
  quiet Linux host is a comment on #1501, a PR that changes the store gives the
  numbers of `benches/sim.rs` at its base and at its head, on one machine. The fixes
  (a cursor for each column in the series iterator, and a gap at the front of a chunk)
  wait for a test or acceptance run whose time is spent in the store (#1501). Decided
  by `laptop.architect-2` (2026-10-07T19:40:18Z):
  https://github.com/synnaxlabs/foundation/pull/1448#issuecomment-6045453368. Amended
  by `laptop.architect-2` (2026-10-08T08:19:35Z):
  https://github.com/synnaxlabs/foundation/pull/1837#issuecomment-6055768246, which
  supersedes the paired run against `77e13735` of
  https://github.com/synnaxlabs/foundation/pull/1448#issuecomment-6045453368, and
  turns on `sim` for the bench.
  `connector_influx::sim::serve(listener, tasks, store, database)` is its HTTP front, on
  `connector::http::sim::serve` (HTTP SIM SERVER). `POST /write?db=` (InfluxDB 1) and
  `POST /api/v2/write?bucket=` (InfluxDB 2 and 3) give 204 when the store takes each
  line, and 400 with the text of the store's error when it refuses one. A missing or
  empty `db` or `bucket`, or on `/api/v2/write` a missing or empty `org` and `orgID`,
  gives 400; a `db` or `bucket` other than `database` gives 404, as InfluxDB gives for
  one that does not exist. `precision` is `ns` only, and a missing or empty one is
  `ns`; another gives 400, where InfluxDB scales it, because our writer writes
  nanoseconds only and a wrong precision must fail loud. Another path gives 404, and
  another method on a write path 405. A `content-encoding` that names a coding other
  than `identity` gets 415 and stores nothing, as the store decodes no body. The HTTP
  front checks the path, then the method, then the `content-encoding`, then the query,
  and gives the answer of the first check that fails. It checks no token. `database`
  stays out of `Store`: the 404 is an answer of the HTTP front. The front compares names
  as the query writes them, with no percent-decoding, until the first PR of
  `connector-influx` that writes a name into a query (#1530). Decided by architect-2
  (2026-10-07T16:33:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042291321,
  2026-10-07T16:41:29Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042446508,
  2026-10-07T17:22:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6043097777,
  2026-10-07T18:32:43Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6044310015).
  Supersedes: https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6043097777
  (the 415 sentence of item 5, by 6044310015).
