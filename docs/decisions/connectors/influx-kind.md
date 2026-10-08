- **INFLUX KIND** `connector_influx::Kind` reads `address` and the reader settings
  (READER SETTINGS). `address` is an `http::Uri`, since a `Name` is a mesh name. `parse`
  reads `address` through `connector::http::uri`, so a plan finds an address that
  `send` refuses before a run. `node` puts the kind in its table only in #1734, when
  `run` works, so until then a file with an influx connector gives
  `connector.unknown-kind` at plan. `check` gives no channels, and `discover` no
  documents. Until #1734, `run` fails with `Error::Config` and `influx.not-yet`.
  Decided by `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC). The kind also refuses a path other than empty or `/`, and a
  query, at the value: a path (a proxy prefix) can come later as a compatible change,
  and a refusal cannot. Each diagnostic of a kind names its document
  `connector::kind::NOUN` ("the connector"), as `reader::read` does. Decided by
  `laptop.architect-2` on #1794
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931,
  2026-10-08 05:03 UTC). The code of that refusal is `influx.bad-address`. Proposed by
  `connector` as its address code
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052532087,
  2026-10-08 04:51 UTC), and approved by item 4 of
  https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931
  (2026-10-08 05:03 UTC).
