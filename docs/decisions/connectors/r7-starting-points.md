- **R7 starting points** OPC UA: open62541 compiled in, with our own crypto plugin on
  aws-lc or compiled-in mbedTLS. Modbus, MQTT with Sparkplug B (pass the TCK), and Kafka
  (pure Rust on `kafka-protocol`, rdkafka behind a flag): built sans-I/O. DAQmx and LJM:
  runtime-loaded bindings, NI functions declared by hand. Codecs: built. Crypto: rustls
  with aws-lc-rs and blake3. Tooling: clap, schemars, toml_edit, tracing. Our own thin
  MCP server, Prometheus text output, and InfluxDB line protocol. FIPS build later.
  HTTP: one client for all connectors, on `hyper` (HTTP/1.1 and HTTP/2) over the `env`
  network seam with `rustls`, in the connector component library. InfluxDB, a general
  HTTP connector, alarms, webhooks, and remote write use it. No HTTP parser of our own.
  Every clock read and name lookup of the client goes through `env`, and no Tokio
  feature of `hyper` or `hyper-util` is on. TLS takes a configured CA, and no setting
  turns verification off. Lost: a sans-I/O HTTP/1.1 module in `connector-influx`. The
  person decided on 2026-10-06 ("Approved." "Adding a bunch of crates is fine. Making a
  binary larger is fine." "we should be careful about writing raw HTTP transports.",
  relayed by `advisor`; "Yes I approve", to the coordinator) (#341). #983 (an `httparse`
  reader) closed: the person told `connector` to use the `hyper` client on 2026-10-06.
  `httparse` comes in only as a dependency of `hyper`. The client is HTTP/1.1 only for
  now: `h2` 0.4 reads the OS clock to expire a reset stream, so HTTP/2 turns on only
  when `h2` takes its clock through `env`, by an upstream change. Decided by the
  coordinator with `advisor` on 2026-10-06 (#341). The client keeps one idle connection
  for each origin. It does not reuse one that is idle longer than 90 s (the `hyper-util`
  default), read on the `env` clock, and the next send closes it: the client sends no
  keep-alive, and a firewall or NAT may drop the state of an idle stream. Decided by the
  coordinator with `advisor` on 2026-10-06
  (https://github.com/synnaxlabs/foundation/issues/341#issuecomment-6022322924). A
  request that fails on a reused connection before its response goes once more on a new
  connection, when the connection did not write it, or when its method is idempotent and
  no byte of a response came (RFC 9112, as in Go). The pool key is the origin, and it
  keeps the host name, because a TLS connection is verified for one name and must never
  carry a request for another. Decided by the architect on #1111
  (https://github.com/synnaxlabs/foundation/pull/1111#issuecomment-6031412223). The key
  is the host name in lower case and the port. `influx.` and `influx` are two keys,
  because a resolver may expand a name with no final dot. The client takes only `http`
  today; with TLS, the key also holds the scheme. A new connection looks up the host
  through `env` and tries each address in order, as Go does: each address gets an equal
  share of the time left to the deadline, but at least 2 s or all that is left. A
  refused address moves the dial to the next at once, and no connect starts once the
  time is up. A reused connection does no lookup. Lost: Happy Eyeballs (RFC 8305), which
  needs more code and streams; a separate error variant for a failed lookup, which a
  caller handles as a failed connect; no limit for each address, where one that drops
  the SYN uses the whole timeout. Proposed by `connector` in the plan on #341
  (https://github.com/synnaxlabs/foundation/issues/341#issuecomment-6031334051) and in
  the review of #1135
  (https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6031807435,
  https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6031903363,
  https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6031982713). The key
  text after the #1111 link, the dial rule, and the `Error::Connect` doc: approved by
  `laptop.architect-2` on 2026-10-07
  (https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6032674524).
  A refused URI gives one error for each cause: `Scheme`, `UserInfo`, `Host`, and
  `Port`, checked in that order.
  Text after `]` is part of the host up to a `:`, so `http://[fd00::2]8086/` gives
  `Host` (https://github.com/synnaxlabs/foundation/issues/1179#issuecomment-6032542190).
  A `[` in a host that is not in brackets gives `Host`. A built URI with an empty path
  sends `/`. Lost: one `Uri` variant that holds the URI with its user info removed,
  because the message must name the cause; and a `Uri` whose `Display` drops the user
  info, because `Debug` and the field still hold the password. Decided by
  `laptop.architect-2` on #1159
  (https://github.com/synnaxlabs/foundation/issues/1159#issuecomment-6032370253).
  No error of a refused URI holds text from the URI: a `/` or `?` in a password ends
  the authority early and puts the password in the host or the port.
  `connector::http::uri` reads a config value as a URI that `send` takes, with no I/O.
  Each refusal, and a fragment, which `send` never sends, is `connector.bad-uri` at the
  value. Lost: a `check(&Uri)` and a code for each kind, which each HTTP kind repeats.
  Decided by `laptop.architect-2` on #1794
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931,
  2026-10-08 05:03 UTC). Supersedes the `Host` and `Port` fields and messages of
  https://github.com/synnaxlabs/foundation/issues/1159#issuecomment-6032370253.
