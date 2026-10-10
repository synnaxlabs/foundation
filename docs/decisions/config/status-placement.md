- **STATUS PLACEMENT (#1821, 2026-10-10)** The status channels of a connector are placed
  with it (X24, X33). `config::plan::plan` places the index `<c>.status.time` with the
  placement of the connector `c`: its winner, home, standby, and copies. The data
  channels of the status follow their index, as each data channel does. Lost: an
  ordinary index, which makes each file that places a connector also select
  `<c>.status.time`, and a pattern of `spec::placement` that selects the status names
  with the connector, a special case for one kind of name in a crate that knows no
  connectors. Only `config` places indexes, so no other crate changes. Decided by
  `laptop.architect-2` (2026-10-10T15:41:24Z,
  https://github.com/synnaxlabs/foundation/issues/1821#issuecomment-6099218399).
  Placement selection does not consider a status channel, so a pattern such as
  `plant.*` that matches a status name changes nothing, and a placement whose `select`
  matches only status names places nothing, as any placement that selects no channel,
  with no diagnostic (`laptop.architect-2`, 2026-10-10T16:07:40Z,
  https://github.com/synnaxlabs/foundation/issues/1821#issuecomment-6099496528, which
  supersedes the `config.empty-placement` clause of
  https://github.com/synnaxlabs/foundation/issues/1821#issuecomment-6099218399). Only
  the connector writes its status: `config.implied-channel` refuses each connector whose
  kind writes a status channel (#2295), so the index can have no other home, and a rule
  that lets a placement give it one only makes an error that no file can fix (ruling
  owed, https://github.com/synnaxlabs/foundation/issues/1821#issuecomment-6101711998).
  The connector is a writer of each index that it implies (PLAN SURFACE).
