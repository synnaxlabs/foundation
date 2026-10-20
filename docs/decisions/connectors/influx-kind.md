- **INFLUX KIND** `connector_influx::Kind` reads `address` and the reader settings
  (READER SETTINGS). `address` is an `http::Uri`, since a `Name` is a mesh name. `parse`
  reads `address` through `connector::http::uri`, so a plan finds an address that `send`
  refuses before a run. Decided by `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC). `discover` gives the default `connector.no-discover`
  (`laptop.architect-2`, 2026-10-16 04:00 UTC:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6207771367; it
  supersedes
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152, where
  `discover` gave no documents). The address takes the scheme `http` or `https` (#2702).
  An optional `ca` is a source of a secret (SECRET SOURCES) that holds PEM certificates,
  so `plan` refuses a `ca` with no value (`secret.unresolved`), as it does a `token`. An
  `https` run trusts the roots of the node and each certificate of the `ca`, and checks
  the server's certificate at mesh time (`https` in R7 starting points). A `ca` with an
  `http` address is `influx.ca-with-http`, with "the connector has a `ca`, which only an
  `https` address takes" and the fix "Use an `https` address, or remove `ca`": a `ca`
  there trusts nothing, and its user likely thinks that the token goes under TLS. A run
  reads the `ca` at its start. A TLS failure of a run with a `ca` shows "the write to
  InfluxDB failed: {error}. Check the ca {source}", and the run reads the `ca` again,
  with a new client, before the next attempt of the same batch, as it reads the token
  again after a 401. A run with no `ca` retries a TLS failure in the run. `Error::Tls`
  also covers a server that closes in the handshake, so such a fault shows "Check the
  ca" too. Approved by `laptop.architect-2` (2026-10-16T16:46:18Z:
  https://github.com/synnaxlabs/foundation/pull/2888#issuecomment-6220307355). Only the
  reads at the start of a run end it. A read of the token or the `ca` after the start
  that fails, with any error of `Context::secret` or of the `ca` text, shows the text of
  that error, as `Context::error` does, with the class `retry`, so the text of a `Retry`
  starts with "an attempt failed: ", which the end of a run at a read at the start does
  not show. It keeps the batch and the old token or client, and reads again after the
  next backoff, with no request until the read gives a value. A config that cannot work
  shows its error until the user fixes it. It loses no sample at the read, but as in an
  outage of InfluxDB, `run` stops reading at a full batch (STORE TRIM and INFLUX SEQ AND
  GAPS give what a sample that it did not read can lose). Decided by
  `laptop.architect-2` on 2026-10-16T17:09:47Z
  (https://github.com/synnaxlabs/foundation/issues/2899) and 2026-10-16T17:09:59Z
  (https://github.com/synnaxlabs/foundation/pull/2888#issuecomment-6220720459), with the
  text approved on 2026-10-16T23:14:40Z
  (https://github.com/synnaxlabs/foundation/issues/2899#issuecomment-6226448986),
  2026-10-17T00:54:51Z
  (https://github.com/synnaxlabs/foundation/pull/2962#issuecomment-6227631111), and
  2026-10-17T01:27:07Z
  (https://github.com/synnaxlabs/foundation/pull/2962#issuecomment-6227945170).
  Supersedes "a config that cannot work ends it" of 6220307355 for a read after the
  start. Lost: the next attempt with the old value, and the read again only after it
  fails, since the shown error then flips between the read and the answer.
  Supersedes item 1 of 6219779608: the end of the run at a TLS failure with a `ca`
  (`laptop.architect-2`, 2026-10-16T16:17:47Z:
  https://github.com/synnaxlabs/foundation/pull/2888#issuecomment-6219779608). A `ca`
  that holds no PEM certificate gives `Error::Retry` with "the ca {source} holds no PEM
  certificate. Write the CA certificate of the server in PEM", and one that does not
  parse gives "the ca {source} holds a PEM certificate that does not parse. Write the
  CA certificate of the server in PEM". Over `https`, the token does not cross the
  network in clear. Planned on #2702
  (https://github.com/synnaxlabs/foundation/issues/2702#issuecomment-6218838848,
  2026-10-16T15:28:25Z), and approved with the refusal of a `ca` with an `http` address
  by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2702#issuecomment-6219190400,
  2026-10-16T15:47:32Z), which also approved `Error::Retry` for a bad `ca`.
  Supersedes the refusal of `https` until the kind takes a CA (`laptop.architect-2`,
  2026-10-16T07:51:33Z:
  https://github.com/synnaxlabs/foundation/pull/2844#issuecomment-6210764044, item 5)
  and its code (`laptop.architect-2`, 2026-10-16T09:11:38Z:
  https://github.com/synnaxlabs/foundation/pull/2844#issuecomment-6212137276), and
  design item 6 of https://github.com/synnaxlabs/foundation/issues/2702, "A `ca` with
  no certificate in it is `Error::Config`". Lost: `ca` as a setting of `connector`,
  read for each HTTP kind, with the read again at a TLS failure, since `influx` is the
  one HTTP kind. Trigger: a second kind that sends `https` (#2894). Lost: a read of the
  `ca` in `plan`, which finds a bad file before a run, since `plan` checks only that a
  source resolves, and the file can change between plan and run. Trigger: a `plan` that
  reads the value of a secret source. The kind also refuses a path other than empty or
  `/`, and a query, at the value: a path (a proxy prefix) can come later as a compatible
  change, and a refusal cannot. Each diagnostic of a kind names its document
  `connector::kind::NOUN` ("the connector"), as `reader::read` does. Decided by
  `laptop.architect-2` on #1794
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931,
  2026-10-08 05:03 UTC). The code of that refusal is `influx.bad-address`. Proposed by
  `connector` as its address code
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052532087,
  2026-10-08 04:51 UTC), and approved by item 4 of
  https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931 (2026-10-08
  05:03 UTC). The kind also reads one target (`Target`): `database` for InfluxDB 1, or
  `bucket` and `org` for InfluxDB 2 and 3. Supersedes "InfluxDB 1 only" of section 4 of
  the ONE NODE record
  (https://github.com/synnaxlabs/foundation/issues/1737#issuecomment-6135595217). Each
  is a string of 1 to 4,096 bytes (`influx.bad-database`, `influx.bad-bucket`,
  `influx.bad-org`), so the query of the write URI holds both names when each byte is
  encoded. A `database` with a `bucket` is `influx.two-targets`, and an `org` with a
  `database` is `influx.org-with-database`. No target, or a `bucket` with no `org`, is
  `document.missing-attribute`. A `database` is written with `POST
  /write?db=<database>&precision=ns`, and a `bucket` with `POST
  /api/v2/write?org=<org>&bucket=<bucket>&precision=ns`, which InfluxDB 2 and InfluxDB 3
  Core take. InfluxDB 3 takes any `org`. The query is encoded as a form with
  `form_urlencoded`: a space is `+`, and each other byte outside `A-Z a-z 0-9 * - . _`
  is `%XX` (#1530). An optional `token` is a source of a secret (SECRET SOURCES), so
  `plan` on the node of the connector, by a subject that may apply it, refuses a
  `token` with no value (`secret.unresolved`). Each request sends it as
  `Authorization: Token <value>`, which InfluxDB 2 and 3 take, and a request with no
  `token` has no `Authorization`. `run` reads the token at its start, and again after
  a 401 or a 403 before the next attempt. This is option A of key choice Q6
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6154974514,
  2026-10-13T20:52:58Z), which item 9 of the ruling of `laptop.architect-2` kept on
  2026-10-13T21:05:46Z
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6155189112). With a
  `token`, a 401 or a 403 gives "InfluxDB at {host} answered {status}. Check the token
  {source}: {body}", so that the cut of a long error keeps the status. With no
  `token`, "InfluxDB at {host} answered {status}: {body}". `{host}` is the host of
  `address`, with its port when `address` names one, so each error names the device
  (condition of `laptop.monitor`,
  https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6241940271). The
  texts with `{host}`: `laptop.architect-2`, 2026-10-17T23:01:30Z
  (https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6242754079). They
  supersede "InfluxDB answered ..." of the two approvals below. A token that holds a
  byte that an HTTP header cannot hold gives `Error::Retry`.
  Approved by `laptop.architect-2` on 2026-10-14T04:16:45Z
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6160767913); the
  401 and 403 text on 2026-10-14T20:02:29Z
  (https://github.com/synnaxlabs/foundation/pull/2687#issuecomment-6177206734).
  Supersedes the key choice of no `Authorization` until this token
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6156072634).
  Approved by the person (2026-10-13T16:24:35Z, relayed in
  https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6150321949), and the
  plan that `laptop.architect-2` approved on 2026-10-13T16:53:54Z
  (https://github.com/synnaxlabs/foundation/issues/2583#issuecomment-6150691934), its
  review of #2610 on 2026-10-13T21:26:57Z
  (https://github.com/synnaxlabs/foundation/pull/2610#issuecomment-6155556749), and its
  approval of the bound of 4,096 bytes on 2026-10-13T21:36:41Z
  (https://github.com/synnaxlabs/foundation/pull/2610#issuecomment-6155716038). The kind
  also reads `measurement`, a name that `line::Measurement::new` takes other than
  `foundation_gaps` (`influx.bad-measurement`). `parse` builds the `line::Measurement`
  once, and the run takes it. `Config` is opaque, as in `opcua` and `modbus`: only
  `parse` makes one, so each `Config` holds checked names (`laptop.architect-2`,
  2026-10-16T17:27:53Z:
  https://github.com/synnaxlabs/foundation/issues/2555#issuecomment-6221027616).
  #2888 merged first, so #2906 makes `Config::ca` `pub(crate)` too. `check`
  gives the reader of its settings and no written channel.
  `run` writes each frame of the reader to the write path of its target: one line for
  each sample time, with a field for each channel of the frame, keyed by the channel
  name. A NaN or infinite value is left out, and so is a line with no other value. A
  `bool` channel is a boolean field. An `i8` to `i64`, `u8` to `u32`, `timestamp` (ns
  since the Unix epoch), or `duration` (ns) channel is an `i` integer. So is a quality
  channel (`DataType::Quality`), a choice that the person has (LINE TEXT). An `f32` or
  `f64` channel is a float, where an `f32` is the `f64` of the same value (0.1 as
  1.0000000149011612e-1), so a reader that narrows it gets back the same bits. A `u64`
  channel is a `u` integer to a `bucket`, and to a `database` it ends the run with
  `Error::Config` and `influx.unsigned` (LINE TEXT). A `string` channel is a string
  field, with `\` before each `"` and `\`, and each other char kept, a newline too. The
  docs of InfluxDB 1, 2, and 3 say that a field value cannot hold a newline, but the
  parsers keep it in a quoted string field, and read only these two escapes in it:
  `scanLine` of `models/points.go` in InfluxDB 1.8.10 (line 1144) and 2.7.12 (line 1170)
  ends a line only at a newline outside quotes, and so does `split_lines` of
  `influxdb-line-protocol` 2.0.0 (`src/lib.rs`, line 614), whose `field_string_value`
  (line 800) reads the string. The send test of a string with a newline pins the parser
  of InfluxDB 3 (`influxdb-line-protocol` 2.0.0). For InfluxDB 1 and 2 this rests on the
  source cited; no test runs them. `laptop.monitor` let the key choices of this
  paragraph go on 2026-10-16T19:28:14Z, on the condition of this text and of the count
  `oversized`
  (https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223099472). A `uuid`
  channel is a string field of 36 lowercase chars, its 16 bytes in RFC 9562 order, so
  byte 0 gives the first two hex chars (S5). A `bytes` channel is a string field of its
  base64, the standard alphabet with padding. A string field whose text between its
  quotes, after the escapes, is over 65,536 bytes (a `bytes` sample over 49,152) is left
  out, and the run goes on. InfluxDB 3 refuses a string field over 65,536 bytes, and the
  docs of InfluxDB 1 and 2 give 64 KB. The count is of the text as written, so each
  version takes each line that the run sends. The count `oversized` gives each sample
  left out, after the 204 of the request of its frame's last line, or with no request
  when no line waits for one. Lost: cut the string (a changed value with no sign), or
  end the run (one long sample stops a stream). Decided by `laptop.architect-2` on
  2026-10-16T17:35:01Z
  (https://github.com/synnaxlabs/foundation/issues/2555#issuecomment-6221149361), on the
  plan of https://github.com/synnaxlabs/foundation/issues/2555#issuecomment-6221118220.
  The run reads a sample of a raw `string` or `bytes` series with
  `codec::Variable::sample` (`laptop.architect`, 2026-10-16T17:33:48Z:
  https://github.com/synnaxlabs/foundation/issues/2901#issuecomment-6221128456). Each
  other type (`array`, `list`, `matrix`) ends the run with `Error::Config` and
  `influx.sample-type`, and a channel name that is not a field key with
  `influx.bad-channel`. The run picks the conversion of a channel with its field, at the
  first frame of each key set that holds it. Lost: an `f32` as its shortest text
  (`1e-1`), as InfluxDB then holds the `f64` 0.1, which is not the value of the sample.
  The types: the type table of `laptop.architect-2` on 2026-10-15T16:13:56Z
  (https://github.com/synnaxlabs/foundation/issues/2555#issuecomment-6196722652),
  approved by the person without arrays on 2026-10-15T16:51:53Z
  (https://github.com/synnaxlabs/foundation/issues/2555#issuecomment-6197478869); the
  surface: `laptop.architect-2` on 2026-10-16T09:09:18Z
  (https://github.com/synnaxlabs/foundation/issues/2555#issuecomment-6212098264). After
  a `Received::gap` that is not `None`, the run writes its gap line with `path=live`
  before the lines of the frame, at the stamp of the frame's first sample, also when it
  leaves that line out (INFLUX SEQ AND GAPS), in the same batch. The line's `count` is
  the seqs of the gap only. Each request acks the position after the last frame that
  ends in it, and a request that only a part of a frame ends in acks none. A frame
  with no sample gives no gap, and its position stays before the gap that the next frame
  with one gives (`hub`), so the run needs no wait of its own. The person chose this
  path ("Ok, this is fine", 2026-10-16T02:03Z, through `laptop.monitor`:
  https://github.com/synnaxlabs/foundation/pull/2794#issuecomment-6206366130), and
  `laptop.architect-2` ruled it on 2026-10-16T02:04:28Z
  (https://github.com/synnaxlabs/foundation/pull/2794#issuecomment-6206384923). It
  supersedes ruling 2 of
  https://github.com/synnaxlabs/foundation/pull/2794#issuecomment-6206327837, a gap that
  waits for a line with a value. One request is in flight at a time. While it is, the
  run reads the next batch. A request body holds at most 5,000 lines, and at most
  4 MiB of them or one line: before each line, a body at that bound closes, and a line
  that takes a body of other lines past 4 MiB moves to the next body. So the lines of
  one frame can span requests (#2918), and the largest request is 4 MiB or one line.
  `laptop.architect-2` set the bound of 4 MiB on 2026-10-16T19:34:27Z
  (https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223208198), under
  the body limit of InfluxDB 3 Core (10 MiB) and that of InfluxDB 1 (25 MB), with 6 MiB
  left for the last frame of a batch. It supersedes a frame that goes in whole of that
  ruling and of the Go of `laptop.monitor` (2026-10-16T19:31:19Z,
  https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223151793), and the
  ack of each batch at its last frame of item 2 of 6206384923 (approved by
  `laptop.architect-2`, 2026-10-17T03:26:02Z,
  https://github.com/synnaxlabs/foundation/pull/2972#issuecomment-6229020580).
  The run keeps the field of each channel that it saw in the frames of one key set. A
  frame of another key set builds them again, as the `hub` reader builds its lens, so
  frames that alternate key sets build them at each change. The trigger to keep them for
  each key set is a workload whose frames alternate key sets, as `laptop.architect-2`
  decided on 2026-10-13T07:36:47Z
  (https://github.com/synnaxlabs/foundation/pull/2552#issuecomment-6140691273). It acks
  a frame only after the 204 of the request that holds its last line, so a restart
  within the `hold` of the reader before that sends the whole frame again, and
  InfluxDB keeps one point for each series, time, and field. The resend:
  `laptop.architect-2` on 2026-10-16T20:16:11Z
  (https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223891016); the ack
  of each frame: the plan of #2918
  (https://github.com/synnaxlabs/foundation/issues/2918#issuecomment-6228349089) and
  `laptop.architect-2` on 2026-10-17T03:26:02Z
  (https://github.com/synnaxlabs/foundation/pull/2972#issuecomment-6229020580). Each
  other answer, and each failed request, keeps the request, shows its text with
  `Context::error`, and sends the request again after a `retry::Backoff` of 1 s to 1
  min. No answer or failed request ends the run: an action in InfluxDB fixes a 400, a
  404, a 413, or a 422, and the next try after it lands the request. The reader stays
  open meanwhile, as in an outage of InfluxDB. The text is `InfluxDB at {host} answered
  {status}: {body}` (the same ruling of 2026-10-17T23:01:30Z), and for these four
  statuses it adds a fix. A 400 and a 422 (InfluxDB 1 gives 400, and InfluxDB 2 gives
  422, for a field type conflict and for points past retention): "For a field type
  conflict, write to another `measurement` or delete the old field in InfluxDB. For
  points older than the retention period, raise it or write to another bucket". A 404:
  "Create the database, the bucket, or the org in InfluxDB, or name one that exists". A
  413: "Raise the request size limit of InfluxDB (`--max-http-request-size` of InfluxDB
  3, `max-body-size` of InfluxDB 1), or write fewer channels or shorter samples at each
  sample time" (the condition of `laptop.monitor` on the batch bound,
  2026-10-16T19:31:19Z,
  https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223151793), with its
  words of 2026-10-17T03:20Z
  (https://github.com/synnaxlabs/foundation/pull/2972#issuecomment-6228984070). An
  answer whose body is over the cap of 64 KiB has its first 64 KiB as `{body}`, as
  `connector::http::Error::TooLarge` holds them, and the fix of its status
  (`laptop.architect-2`, 2026-10-16T07:38:36Z,
  https://github.com/synnaxlabs/foundation/pull/2838#issuecomment-6210559216). Decided
  by `laptop.architect-2` on 2026-10-16T07:18:54Z
  (https://github.com/synnaxlabs/foundation/pull/2838#issuecomment-6210258673), which
  replaces the stop at a 400, a 404, or a 422 of the ruling of 2026-10-14T01:32:02Z
  (https://github.com/synnaxlabs/foundation/issues/2607#issuecomment-6158874569).
  QUARANTINE, which lets the connector go on past refused points, is a later step
  (#2841). A 401 and a 403 keep the backoff, since the run reads the token again before
  the next try (#2687). A 413 keeps the backoff too, so a raise of the limit lets the
  held frames go with no sample lost (`laptop.monitor`, 2026-10-16T19:31:19Z,
  https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223151793). The
  split of a frame across requests has the trigger of `laptop.architect-2`
  (2026-10-16T19:34:27Z,
  https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223208198): a frame
  of over 6 MiB of lines, which a ring record of 1 MiB of 64 `u8` channels gives (about
  13 MiB of lines). It supersedes the trigger of a split of the batch, a server whose
  body limit is below one batch, of
  https://github.com/synnaxlabs/foundation/pull/2838#issuecomment-6210258673.
  `laptop.monitor` admitted #2918 as a stall at the defaults
  (2026-10-16T20:16:04Z,
  https://github.com/synnaxlabs/foundation/issues/2918#issuecomment-6223888984). A cut
  of a batch before such a frame lost, since a frame over the whole limit still stops
  the stream (`laptop.architect-2`, 2026-10-16T20:16:11Z,
  https://github.com/synnaxlabs/foundation/pull/2906#issuecomment-6223891016). Only a
  single line over the body limit of the server, or a body limit under 4 MiB, can still
  take a request past it.
  A 204 clears the error, and adds the fields of each frame that it acks to the count
  `confirmed` with `Count::add`, and the `count` of each gap line of those frames to the
  count `lost`: the seqs that the gap lines of this connector count, which InfluxDB took
  with a 204. A request sent again counts once. A part of a frame that lands before
  the run ends, with the frame not acked, adds to no count, so with no `hold`
  `confirmed` and `lost` can be below what InfluxDB holds. The counts of each frame that
  a 204 acks supersede the counts of each batch of item 2 of 6199058253
  (`laptop.architect-2`, 2026-10-17T03:26:02Z,
  https://github.com/synnaxlabs/foundation/pull/2972#issuecomment-6229020580). A gap
  that the reader reports again after a restart counts again, as the sum of `count` in
  `foundation_gaps` does. Each later out kind names the same count `lost`. It counts
  sample times of the reader's index. The `lost` of `opcua` counts data samples (OPCUA
  KIND). The ruling did not take a count of all out kinds in `connector`, as the
  supervisor does not see the frames of a kind's reader. The trigger to make it is a
  second out kind (6199058253).
  The three counts hold across the runs of one call (SUPERVISOR). The person gave the
  choice of the path to the architects on 2026-10-15 ("Best devx possible. You choose
  the path", through `laptop.monitor`:
  https://github.com/synnaxlabs/foundation/issues/2553#issuecomment-6199035922).
  `laptop.architect` gave the count of lost seqs, and `laptop.architect-2` ruled its
  shape on 2026-10-15T18:13:22Z
  (https://github.com/synnaxlabs/foundation/issues/2553#issuecomment-6199058253) and the
  gap lines of the run on 2026-10-15T18:20:15Z
  (https://github.com/synnaxlabs/foundation/issues/2725#issuecomment-6199190890). A NaN
  or infinite sample writes no field and is not counted (#2559, the plan that
  `laptop.architect-2` approved on 2026-10-13T09:52:20Z:
  https://github.com/synnaxlabs/foundation/issues/2559#issuecomment-6142931456). A
  request that takes longer than the timeout of 10 s is sent again with no end, until
  #2558, as `laptop.architect-2` decided on 2026-10-13T06:20:10Z
  (https://github.com/synnaxlabs/foundation/pull/2552#issuecomment-6139570127). A
  restart within the `hold` of the reader sends each frame not acked again: the next run
  reads its frames again from disk (#274 B1; `laptop.architect-2`, 2026-10-15T18:05:36Z:
  https://github.com/synnaxlabs/foundation/pull/2723#issuecomment-6198895692). With no
  `hold`, or after it, the next run reads from the live frames, so those lines are lost.
  So a part of a frame can stay in InfluxDB while the rest of it is lost, as with
  Telegraf, which splits by line, and as with a 400 of InfluxDB, which keeps the good
  lines of its request. `laptop.monitor` agreed this on 2026-10-17T03:20Z
  (https://github.com/synnaxlabs/foundation/pull/2972#issuecomment-6228984070).
  Decided by the ONE NODE record, section 4, on 2026-10-13T00:50:41Z
  (https://github.com/synnaxlabs/foundation/issues/1737#issuecomment-6135595217), and
  the plan that `laptop.architect-2` approved on #1734
  (https://github.com/synnaxlabs/foundation/issues/1734#issuecomment-6135905432).
