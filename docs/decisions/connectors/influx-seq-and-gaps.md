- **INFLUX SEQ AND GAPS (#1151)** The InfluxDB out connector stores no seq. A stamp
  names one sample of an index on each path (X31), and InfluxDB keys a point by
  measurement, tag set, and time, so a resend stores each sample once. Each run of
  explicit gaps before a sample is one line,
  `foundation_gaps,connector=<connector>,index=<index>,path=<path> count=<n>i <stamp>`:
  `<path>` is `live` or `backfill` (amendment below), `<stamp>` is the stamp of the
  first sample after the gaps, and `count` is the number of seqs from the first trimmed
  seq up to that sample. The gap line goes in the request of that sample, and the
  position is acked only after InfluxDB confirms it (B3). The count is signed, because
  InfluxDB 1 OSS refuses `u`. The `connector` tag keeps two connectors that write one
  index to one database from replacing each other's gap lines. Until a later sample
  comes, the connector keeps one gap per index, and from #1270 one per index and path.
  After a restart the buffer reports the gap again (READER RULES), so a lost gap line is
  sent again. The measurement name is fixed. #1734 names the data measurement, and its
  kind check refuses `foundation_gaps` as one (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC). Supersedes the clause of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032215953 that the
  kind check (#1153) refuses a config that maps a data measurement to `foundation_gaps`.
  Fold rule (6032756428, which replaces the fold rule of 6032215953): `Lab::stored`
  reads each gap line as the seqs `[seq(stamp) - count, seq(stamp))`, where
  `seq(stamp)` is the seq of the data point at its stamp, in the data measurement of
  the same index (6039993275: stamps slew, so a stamp is not a key into the write
  record). Seqs rise with time: a point whose seq is not above the seq before it is a
  lab failure, and the message names both stamps and both seqs. Its gaps are the
  union of these ranges minus the stored seqs, as maximal runs. Each run is one gap:
  `after` is the count of stored samples before the run, and the count is the run's
  length. A gap line with no data point at its stamp, or whose range starts below the
  first written seq, is a lab failure (panic), not data. The property test
  also asserts no silent loss: each seq from the first written seq to the last stored
  seq is stored or in a gap range. After a lost confirmation, a resend, and a later
  trim, gap lines can overlap, so the sum of `count` in `foundation_gaps` is an upper
  bound on the loss from trims. Before #1270 (amendment below) that is the whole loss,
  and the exact loss is the union of the ranges minus the stored samples.
  Lost: a seq field (about 20 bytes a line), a seq tag (one series per sample), a gap
  point in the data measurement (a field type conflict), a configurable gap
  measurement, and a `first=<seq>i` field on each gap line. That field puts a seq,
  which is internal to the node, into each user's InfluxDB; the lab does not need
  it; a reader still cannot get the exact loss, as the stored samples hold no seq;
  and it makes each gap line longer when the store is under pressure. Decided by the
  architect (`laptop.architect-2`), #1151
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032215953,
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032474515,
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032756428,
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032802085), and
  in the review of #1225
  (https://github.com/synnaxlabs/foundation/pull/1225#issuecomment-6032761284,
  https://github.com/synnaxlabs/foundation/pull/1225#issuecomment-6032817985).
  Amended on #1151: with #1270 (M2), the connector is a recording reader and writes the
  samples of both paths (A6, A8). Until then it reads the live path only, and the
  store-and-forward tests use the live path only. The `path` tag of each gap line is
  `live` until then, so the format does not change at M2. The `path` tag keeps a live
  gap line and a backfill gap line at one stamp as two points, so the sum of `count`
  stays an upper bound on the loss from trims. Data lines get no `path` tag, so a live
  sample and a backfill sample of one index at one stamp are one point, and the later
  write sets its fields. The earlier sample is a loss that no gap line counts. Until
  #1270, the fold reads the live path only, and a gap line of another path is a lab
  failure. #1270 amends the fold for two paths. From #1270, a separate test reads the
  points of the simulated InfluxDB and pins the overwrite. Lost: a `path` tag on data
  lines, which makes two series for each channel and puts the path, which is internal to
  the node, in each user's data schema. `foundation_gaps` is Foundation's own
  measurement, so its `path` tag costs the user's data nothing. Decided by
  `laptop.architect-2` on #1151 (2026-10-07T08:07:33Z:
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6033743748; amended
  2026-10-07T08:18:10Z:
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6033910167;
  corrected 2026-10-07T16:52:35Z:
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6042660446).
  Supersedes the gap line with no `path` tag of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032474515 and of
  point 3 of 6033743748, and the loss bound of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032756428.
  Data points in the lab: sample `k` of one `Lab::write`, from 0, has the value
  `k as f64`, which is exact below 2^53. `Lab::stored` takes a data point's seq from
  its value: `written.start + k`. It accepts a data point only when its fields are
  one float that is a whole number `k` in `+0..count`, where `count` is the number
  of written samples. Until #341 names the field key of a data line, the field may
  have any key; the #341 PR that names the key changes the check to that key.
  Decided by the architect (`laptop.architect-2`), #1151, on 2026-10-07T14:07:11Z
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6039706938)
  and 2026-10-07T14:22:17Z
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6039993275).
  Supersedes the Q2 check of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6039706938, which
  compared each value with `seq - written.start`.
